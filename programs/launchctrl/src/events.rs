use anchor_lang::prelude::*;

#[event]
pub struct LaunchCreated {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub name: String,
    pub symbol: String,
    pub launch_timestamp: i64,
}

#[event]
pub struct MigrationComplete {
    pub mint: Pubkey,
    pub migration_timestamp: i64,
}

#[event]
pub struct MeteoraPoolCreated {
    pub mint: Pubkey,
    pub meteora_pool: Pubkey,
    pub sol_deposited: u64,
    pub tokens_deposited: u64,
    pub drip_total: u64,
}

#[event]
pub struct LpReinjected {
    pub mint: Pubkey,
    /// Q64.64 liquidity units deposited back into the locked position.
    pub liquidity_delta: u128,
    /// Tokens sitting in lp_seed_token_vault immediately before add_liquidity.
    pub lp_seed_tokens_before: u64,
    /// wSOL collected from claim_position_fee (the SOL-side LP fees since last crank).
    pub wsol_claimed: u64,
    /// 4-way split applied to wsol_claimed (FEE_REWORK April 2026 / NEST_EGG_TO_DEV_BONUS Phase 3).
    pub lp_take: u64,         // 62.5%
    pub reward_take: u64,     // 25% (boosted to 31.25% when dev_bonus_eligible == false)
    pub dev_bonus_take: u64,  // 6.25% — replaces nest_egg_take
    pub platform_take: u64,   // 6.25%
    /// True if the creator still holds at least their `initial_buy_amount`.
    /// When false, `dev_bonus_take` is rolled into the holder pool for this cycle
    /// (so the actual reward_vault delta = reward_take + dev_bonus_take).
    pub dev_bonus_eligible: bool,
}

#[event]
pub struct CurveInitialized {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub migration_threshold_lamports: u64,
}

#[event]
pub struct TokensBought {
    pub mint: Pubkey,
    pub buyer: Pubkey,
    pub sol_in: u64,
    pub tokens_out: u64,
    pub platform_fee: u64,
}

#[event]
pub struct TokensSold {
    pub mint: Pubkey,
    pub seller: Pubkey,
    pub tokens_in: u64,
    pub sol_out: u64,
    /// Total fee at this trade's decay tier, in basis points. ≥ BASE_FEE_BPS.
    pub total_fee_bps: u64,
    /// Lamports routed to lp_seed_sol_vault (base fee + 50% of decay component).
    pub retained_sol_lamports: u64,
    /// Lamports used as input to the inline buyback (50% of decay component).
    /// Zero outside decay or when curve is_complete.
    pub buyback_sol_lamports: u64,
    /// Tokens produced by the inline buyback and routed to lp_seed_token_vault.
    pub buyback_tokens: u64,
}

#[event]
pub struct CurveComplete {
    pub mint: Pubkey,
    pub final_sol: u64,
}

#[event]
pub struct FundsReleased {
    pub mint: Pubkey,
    pub sol_amount: u64,
    pub token_amount: u64,
}

#[event]
pub struct RewardsDiverted {
    pub mint: Pubkey,
    pub amount_lamports: u64,
    pub reward_vault: Pubkey,
}

#[event]
pub struct RewardsWithdrawn {
    pub mint: Pubkey,
    pub recipient: Pubkey,
    pub amount_lamports: u64,
}

/// Emitted on every claim_and_reinject cycle where dev_bonus_take > 0.
/// `eligible = true`  → `amount_lamports` flowed to `deployer_vault`.
/// `eligible = false` → `amount_lamports` rolled into the Diamond Hands holder
///                       pool that cycle (creator forfeited their slice).
/// `deployer_vault` field is the canonical vault PDA in either case (so
/// indexers can correlate forfeitures with the launch they belong to).
#[event]
pub struct DevBonusDiverted {
    pub mint: Pubkey,
    pub amount_lamports: u64,
    pub eligible: bool,
    pub deployer_vault: Pubkey,
}

/// Emitted when a launch's creator successfully claims the one-shot 5 SOL
/// bootstrap. Fires exactly once per launch; subsequent attempts revert with
/// `BootstrapAlreadyClaimed`.
#[event]
pub struct BootstrapClaimed {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub amount_lamports: u64,
}

/// Emitted on a successful `admin_withdraw_deployer_bonus`. Mirrors
/// `RewardsWithdrawn` but for the per-mint `deployer_vault`. Fires every time
/// the rewards authority drains the creator's accumulated dev-bonus share
/// (typically once per 10-day Diamond Hands cycle, on top of the regular
/// holder claim).
#[event]
pub struct DeployerBonusWithdrawn {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub amount_lamports: u64,
}

#[event]
pub struct RicochetConfigInitialized {
    pub mint: Pubkey,
    pub expires_at: i64,
    pub duration_seconds: u32,
}
