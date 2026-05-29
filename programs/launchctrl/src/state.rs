use anchor_lang::prelude::*;

// ─── Launch state ────────────────────────────────────────────────────────────

#[account]
pub struct LaunchState {
    pub creator: Pubkey,                    // 32
    pub mint: Pubkey,                       // 32
    pub name: String,                       // 4 + 32
    pub symbol: String,                     // 4 + 10
    pub uri: String,                        // 4 + 200
    pub total_supply: u64,                  // 8
    pub decimals: u8,                       // 1
    pub launch_timestamp: i64,              // 8
    /// Sell-side decay schedule. Evaluated in real time against `launch_timestamp`
    /// inside `sell.rs` — no more crank + no more on-chain `decay_index` tracking.
    pub decay_schedule: Vec<DecayStep>,     // 4 + 10 * 10 = 104
    pub migration_threshold_lamports: u64,  // 8
    pub is_migrated: bool,                  // 1
    pub bump: u8,                           // 1
    pub fee_authority_bump: u8,             // 1
    pub migration_timestamp: i64,           // 8
    pub drip_total: u64,                    // 8 — withheld tokens at migration time (informational)
    pub drip_injected: u64,                 // 8 — reserved, always 0 in current version
    pub meteora_pool: Pubkey,              // 32 — DAMM v2 pool address (set after migration)
    pub drip_vault_bump: u8,               // 1
    pub migration_sol_vault_bump: u8,      // 1
    /// Tokens received by the creator on the very first buy of the curve.
    /// Captured in `buy.rs` only when `initial_buy_amount == 0 && real_sol_reserves == 0
    /// && signer == creator`. Stays 0 forever if the creator skips the launch-tx initial buy.
    /// Drives Diamond-Hands-style "still holding initial" gating for the deployer bonus.
    pub initial_buy_amount: u64,           // 8 — fills 8 of the prior 9-byte padding allocation
    /// One-shot flag for the 5 SOL bootstrap claim. Flips true on first `claim_bootstrap`
    /// success and never resets.
    pub bootstrap_claimed: bool,           // 1 — fills the final byte of the prior padding
}

impl LaunchState {
    pub const LEN: usize = 8    // discriminator
        + 32 + 32               // creator, mint
        + (4 + 32)              // name
        + (4 + 10)              // symbol
        + (4 + 200)             // uri
        + 8 + 1 + 8             // total_supply, decimals, launch_timestamp
        + (4 + 10 * 10)         // decay_schedule (max 10 steps * 10 bytes each)
        + 8 + 1 + 1 + 1         // migration_threshold, is_migrated, bumps
        + 8 + 8 + 8 + 32 + 1 + 1 // migration_timestamp, drip_total, drip_injected, meteora_pool, vault bumps
        + 8 + 1;                // initial_buy_amount + bootstrap_claimed
                                // (consumed the prior 9-byte padding — total LEN unchanged)
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct DecayStep {
    pub seconds_after_launch: u64, // 8
    pub fee_bps: u16,              // 2
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct LaunchParams {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub total_supply: u64,
    pub decimals: u8,
    pub migration_threshold_lamports: u64,
    pub decay_schedule: Vec<DecayStep>,
}

/// Empty marker account — program owns it so raw lamport manipulation
/// is permitted by the runtime.
#[account]
pub struct MigrationSolVault {}

/// Per-mint Diamond Hands rewards accumulator. Zero-data marker owned by the
/// launchctrl program; exists purely so we can raw-debit lamports out via the
/// admin withdraw instruction. Receives 25% of each `claim_and_reinject`
/// cycle's wSOL-side LP-fee share. PDA seeds = [REWARD_VAULT_SEED, mint].
#[account]
pub struct RewardVault {}

/// Per-mint Deployer Bonus accumulator (NEST_EGG_TO_DEV_BONUS rework).
/// Replaces the legacy `NestEggVault` (removed in Phase 3). Receives 6.25%
/// of each `claim_and_reinject` cycle's wSOL-side LP-fee share — but ONLY
/// when the creator still holds at least their `LaunchState.initial_buy_amount`.
/// If they've sold below that threshold, the slice rolls into `reward_vault`
/// for that cycle (boosting the holder pool). PDA seeds = [DEPLOYER_VAULT_SEED, mint].
/// Drained by either `claim_bootstrap` (one-shot 5 SOL) or the regular
/// Diamond-Hands-gated claim flow.
#[account]
pub struct DeployerVault {}

/// Per-mint LP-seed SOL accumulator (FEE_REWORK April 2026).
/// Zero-data marker. Accumulates:
///   - 100% of base sell-fee SOL during the bonding curve
///   - 50% of the decay component from each sell during decay
///   - 62.5% of wSOL-side LP fees claimed post-migration
/// Drained by `claim_and_reinject` to feed `Meteora::add_liquidity`.
/// PDA seeds = [LP_SEED_SOL_VAULT_SEED, mint].
#[account]
pub struct LpSeedSolVault {}

// ─── Curve state ─────────────────────────────────────────────────────────────

#[account]
pub struct CurveState {
    pub mint: Pubkey,                        // 32
    pub creator: Pubkey,                     // 32
    pub virtual_sol_reserves: u64,           // 8
    pub virtual_token_reserves: u64,         // 8
    pub real_sol_reserves: u64,              // 8
    pub real_token_reserves: u64,            // 8
    pub migration_threshold_lamports: u64,   // 8
    pub creation_slot: u64,                  // 8  — BundleGuard: slot token was created
    pub last_buy_slot: u64,                  // 8  — BundleGuard: slot of most recent buy
    pub is_complete: bool,                   // 1
    pub is_funds_released: bool,             // 1
    pub bump: u8,                            // 1
    pub vault_bump: u8,                      // 1
    pub sol_vault_bump: u8,                  // 1
    pub buys_this_slot: u8,                  // 1  — BundleGuard: buy count in last_buy_slot
}

impl CurveState {
    pub const LEN: usize = 8 + 32 + 32 + 8 + 8 + 8 + 8 + 8 + 8 + 8 + 1 + 1 + 1 + 1 + 1 + 1 + 14; // 14 padding
}

/// Empty account — exists purely so the program owns it and can perform
/// raw lamport debit in the `sell` instruction.
#[account]
pub struct SolVault {}

/// Per-token blocklist. Created alongside the curve; creator populates it
/// via `add_to_blocklist` before (or at) launch. Checked on every buy/sell.
///
/// `frozen` flips to true on the first buy (`buy.rs`). After that the
/// creator can no longer add or remove addresses. This is the defining
/// anti-rug invariant for the KOL Shield: list-at-launch, immutable-thereafter.
#[account]
pub struct Blocklist {
    pub mint: Pubkey,           // 32
    pub creator: Pubkey,        // 32
    pub blocked: Vec<Pubkey>,   // 4 + N*32
    pub bump: u8,               // 1
    pub frozen: bool,           // 1
}

impl Blocklist {
    /// Account byte size for N blocked addresses.
    pub fn space(n: usize) -> usize {
        8   // Anchor discriminator
        + 32  // mint
        + 32  // creator
        + 4 + n * 32  // Vec<Pubkey>
        + 1   // bump
        + 1   // frozen
    }
}

/// Per-mint Ricochet enforcement config. Optional: only created if the launch
/// enables Ricochet (via `initialize_ricochet_config`). When this PDA exists
/// and `Clock::get().unix_timestamp < expires_at`, buy.rs and sell.rs run
/// `scan_for_blocked_programs` against the Instructions sysvar.
///
/// Replaces the old per-mint `RicochetEnforce` PDA from the standalone
/// ricochet program. Migration plan: see docs/architecture/RICOCHET_INLINE.md.
///
/// PDA seeds = [RICOCHET_CONFIG_SEED, mint].
#[account]
pub struct RicochetConfig {
    pub mint: Pubkey,        // 32 — explicit even though derivable, lets us assert at runtime
    pub expires_at: i64,     //  8 — absolute Unix timestamp (launch_timestamp + duration_seconds)
    pub bump: u8,            //  1
}

impl RicochetConfig {
    pub const LEN: usize = 8 + 32 + 8 + 1; // 49 bytes total
}
