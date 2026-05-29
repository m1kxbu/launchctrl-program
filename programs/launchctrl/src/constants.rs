use anchor_lang::prelude::*;

// ─── Platform / external programs ────────────────────────────────────────────

/// Platform fee vault — receives buy fees + 6.25% of post-mig LP claims.
/// Squads multisig (2-of-3 threshold on both clusters; vault addresses differ).
/// Mainnet vault = "LaunchCTRL Fee Vault" Squad. Devnet vault preserved for
/// continued devnet testing until full Scope-B mainnet flip.
#[cfg(feature = "mainnet")]
pub const PLATFORM_FEE_VAULT: Pubkey = pubkey!("7JY2zbBfqjsugLYMJyRELqM4wHBUMt2rpTFXnMiwRGQs");
#[cfg(not(feature = "mainnet"))]
pub const PLATFORM_FEE_VAULT: Pubkey = pubkey!("2Do45QcuM3yvfes3Uc5PuD3BcsRZ29pq56LQWQczyDvi");

/// WSOL native mint.
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

/// SPL Token program (classic, not Token-2022) — used for WSOL operations.
pub const SPL_TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Meteora DAMM v2 program — mainnet + devnet address.
pub const METEORA_DAMM_V2_PROGRAM: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");

/// Meteora DAMM v2 pool authority PDA = findPDA(["pool_authority"], METEORA_DAMM_V2_PROGRAM)
pub const METEORA_POOL_AUTHORITY: Pubkey = pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");

/// Meteora DAMM v2 event authority PDA = findPDA(["__event_authority"], METEORA_DAMM_V2_PROGRAM)
pub const METEORA_EVENT_AUTHORITY: Pubkey = pubkey!("3rmHSu74h1ZcmAisVcWerTCiRDQbUrBKmcwptYGjHfet");

/// Meteora DAMM v2 instruction discriminators (verified against the IDL).
pub const DAMM_V2_DISC_INIT_POOL:           [u8; 8] = [20, 161, 241, 24, 189, 221, 180, 2];
pub const DAMM_V2_DISC_PERMANENT_LOCK:      [u8; 8] = [165, 176, 125, 6, 231, 171, 186, 213];
pub const DAMM_V2_DISC_CLAIM_POSITION_FEE:  [u8; 8] = [180, 38, 154, 17, 133, 33, 162, 211];
pub const DAMM_V2_DISC_ADD_LIQUIDITY:       [u8; 8] = [181, 157, 89, 67, 143, 182, 52, 72];

/// DAMM v2 full-range sqrt prices (mirrors Uniswap v3 tick bounds).
pub const SQRT_MIN_PRICE: u128 = 4_295_048_016u128;
pub const SQRT_MAX_PRICE: u128 = 79_226_673_521_066_979_257_578_248_091u128;

/// PoolFeeParameters Borsh-encoded for a 1.0% flat fee (no rate limiter).
///
/// Bumped from 0.5% → 1.0% in the FEE_REWORK redesign because the Token-2022
/// TransferFee extension was removed; the Meteora LP fee is now the entire
/// post-migration trading fee. 1% per side keeps the user-facing 2% round-trip
/// identical to pre-rework, and the LP-fee share funds Diamond Hands rewards,
/// the Community Nest Egg, platform revenue, and continued LP thickening.
///
/// Layout (31 bytes):
///   BaseFeeParameters.data[27] = BorshFeeRateLimiter {
///     cliff_fee_numerator: u64 = 10_000_000  (1.0% of FEE_DENOMINATOR=1e9)
///     fee_increment_bps:   u16 = 0
///     max_limiter_duration: u32 = 0
///     max_fee_bps:         u32 = 0  (all zero = flat fee, rate limiter disabled)
///     reference_amount:    u64 = 0
///     base_fee_mode:        u8 = 0  (flat)
///   }
///   compounding_fee_bps: u16 = 0
///   padding:              u8 = 0
///   dynamic_fee:  Option   = None (discriminant 0)
pub const POOL_FEE_PARAMS: [u8; 31] = [
    // BaseFeeParameters.data — flat fee, all rate-limiter fields = 0
    0x80, 0x96, 0x98, 0x00, 0x00, 0x00, 0x00, 0x00, // cliff_fee_numerator = 10_000_000 (0x989680)
    0x00, 0x00,                                        // fee_increment_bps = 0
    0x00, 0x00, 0x00, 0x00,                            // max_limiter_duration = 0
    0x00, 0x00, 0x00, 0x00,                            // max_fee_bps = 0 (flat fee)
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,   // reference_amount = 0
    0x00,                                              // base_fee_mode = 0 (flat)
    // compounding_fee_bps: u16 = 0
    0x00, 0x00,
    // padding: u8 = 0
    0x00,
    // dynamic_fee: Option = None
    0x00,
];

// ─── Bonding curve fees (FEE_REWORK April 2026) ──────────────────────────────
//
// Replaces the old TRANSFER_FEE_BPS + PLATFORM_FEE_BPS construction. All fees
// are SOL-denominated; new mints have NO Token-2022 extensions. See docs/architecture/FEE_REWORK.md.

/// Base bonding-curve fee in basis points (1.0%). Applied to:
///   - Buy: 100% routes to `fee_vault` (platform revenue)
///   - Sell: 100% routes to `lp_seed_sol_vault` (LP seed accumulator)
/// During decay, the SELL fee can exceed BASE_FEE_BPS — the additional
/// "decay component" is split 50/50 between inline buyback and retained SOL.
pub const BASE_FEE_BPS: u64 = 100;

/// Fraction of the sell-side decay component that drives an inline buyback
/// against the bonding curve. 5000 bps = 50%. The other 50% is retained as
/// SOL in `lp_seed_sol_vault`. Net effect over the curve's lifetime: a
/// balanced (SOL, tokens) LP-seed position ready for `add_liquidity` at
/// migration.
pub const BUYBACK_SPLIT_BPS: u64 = 5000;

/// Default decay schedule. (seconds_after_launch, fee_basis_points).
/// Evaluated in real time inside `sell.rs`. The 100 bps floor is the
/// BASE_FEE_BPS — sells outside any decay window pay exactly 1%.
pub const DEFAULT_DECAY_SCHEDULE: [(u64, u16); 5] = [
    (0,      3000), // 0h:   30%
    (3_600,  2000), // 1h:   20%
    (10_800, 1000), // 3h:   10%
    (21_600,  500), // 6h:    5%
    (43_200,    0), // 12h:   0% (resolves to 100 bps floor)
];

// ─── PDA seeds ───────────────────────────────────────────────────────────────

pub const LP_SEED_SOL_VAULT_SEED:   &[u8] = b"lp_seed_sol";
pub const LP_SEED_TOKEN_VAULT_SEED: &[u8] = b"lp_seed_tok";
/// PDA seed for the per-mint deployer-bonus accumulator (NEST_EGG_TO_DEV_BONUS rework).
/// Replaces the legacy `NEST_EGG_VAULT_SEED = b"nest_egg"` which was removed in Phase 3+.
pub const DEPLOYER_VAULT_SEED:      &[u8] = b"deployer_vault";

// ─── Bonding curve ───────────────────────────────────────────────────────────

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Virtual reserves seed liquidity so early buys aren't free.
/// Mirrors pump.fun's approach: virtual SOL + virtual token reserves.
///
/// Tuned 2026-05-14 to anchor a higher starting MC (~$5.5K at SOL=$91)
/// and a thicker post-bond LP. Diverges from pump.fun's 30/1.073B/85 to:
///   start_MC ≈ $5,491 · migration_MC ≈ $40,463 · start→bond multiplier ≈ 7.4x
/// (vs pump.fun's $2.5K / $37K / 14.7x at the same SOL price). 2.36× deeper
/// virtual SOL anchor doubles the dollar cost of sniping any % of supply.
pub const VIRTUAL_SOL_RESERVES: u64 = 70 * LAMPORTS_PER_SOL;    // 70 SOL virtual
pub const VIRTUAL_TOKEN_RESERVES: u64 = 1_160_000_000_000_000;   // 1.16B tokens (6 dec)
pub const DEFAULT_MIGRATION_THRESHOLD_LAMPORTS: u64 = 120 * LAMPORTS_PER_SOL; // 120 SOL

/// Hard bounds on creator-supplied `migration_threshold_lamports`. Without
/// these, a malicious creator could pass 1 lamport (instant migration on the
/// first buy → buyers stranded on a half-formed curve) or u64::MAX (curve
/// never migrates → all bonded SOL trapped forever, only-other-buyers can
/// rescue). MAX is the load-bearing defense: 200 SOL is well above the
/// frontend's 121 SOL setpoint and rejects the catastrophic trap-forever
/// shape. MIN is loose (1 SOL) — primarily a sanity floor against pathologic
/// `0` / `1 lamport` values. M-1's sqrt-price gate plus the migration token
/// vault residual check (`InsufficientPoolLiquidity`) catch realistic
/// thin-migration attacks regardless of MIN.
pub const MIN_MIGRATION_THRESHOLD_LAMPORTS: u64 = LAMPORTS_PER_SOL;
pub const MAX_MIGRATION_THRESHOLD_LAMPORTS: u64 = 200 * LAMPORTS_PER_SOL;

/// Hard cap on creator-supplied decay-schedule fee_bps. The on-chain `sell`
/// pipeline floors at `BASE_FEE_BPS` (1%) and applies the schedule on top —
/// the previous 10_000 bps cap let a creator launch with `[(0, 10_000)]` so
/// every sell forfeits 100% of proceeds. 5_000 bps (50%) is more than double
/// the frontend max (25%) — generous for legitimate anti-snipe schedules,
/// short of weaponizable as a sell-side rug.
pub const MAX_DECAY_FEE_BPS: u16 = 5_000;

/// Maximum number of decay-schedule entries stored on `LaunchState`. The
/// account's allocated space accommodates up to 10 steps; an explicit cap
/// rejects oversized schedules at the validation layer rather than via an
/// opaque serialization failure.
pub const MAX_DECAY_SCHEDULE_LEN: usize = 10;

/// `create_meteora_pool` post-init invariant: the migration token vault must
/// be drained to within this fraction of its starting balance, otherwise we
/// reject the call (defense against a malicious cranker passing a degenerate
/// `liquidity_delta` and stranding the bulk of migration tokens). Value =
/// 10% (≥90% deposited), well above the bot's typical sub-1% residual.
pub const MAX_POOL_INIT_TOKEN_RESIDUAL_BPS: u64 = 1_000;

/// Maximum number of wallet addresses per token blocklist.
/// Initialized at launch — creator pays rent for the full allocation upfront.
///
/// Reduced 50 → 20 on 2026-05-25. LaunchForm has capped user input at 20
/// since 2026-04-28 (commit 02e47bf, "was 50, exceeded 1232-byte tx limit"),
/// but the program kept allocating for 50 → every launch wasted 6,681,600
/// lamports (~0.0067 SOL) on slots the form couldn't fill. Existing on-chain
/// Blocklists keep their 1678-byte allocation (Anchor `init` is one-shot);
/// only new launches use the smaller 718-byte allocation. Backward-compatible.
///
/// Cross-stack invariant locked by `launchctrl.invariants.test.ts`.
pub const MAX_BLOCKLIST_SIZE: usize = 20;

/// Creator initial buy cap: 2% of VIRTUAL_TOKEN_RESERVES (200 bps).
/// Only enforced on the very first buy when real_token_reserves == 0.
pub const MAX_CREATOR_INITIAL_BUY_BPS: u64 = 200;

/// BundleGuard: tiered per-slot buy limit.
pub const BUNDLE_GUARD_STRICT_SLOTS: u64 = 5;
pub const BUNDLE_GUARD_SOFT_SLOTS:   u64 = 150;
pub const BUNDLE_GUARD_STRICT_MAX:    u8 = 1;
pub const BUNDLE_GUARD_SOFT_MAX:      u8 = 3;

// ─── Post-migration LP-fee split (claim_and_reinject 4-way) ──────────────────
//
// `claim_position_fee` returns wsol_claimed + project_tokens_claimed. The
// project tokens flow 100% into `lp_seed_token_vault` (and feed the next
// add_liquidity). The wSOL side is split 4 ways below; sum = 10_000.
//
// User-facing post-migration fee = 1% per side. Of that 1%, Meteora's
// protocol fee takes 20% (= 0.20% per side, 0.40% round-trip), leaving 80%
// (= 0.80% per side, 1.60% round-trip) as the LP-fee share that reaches us.
//
// We apportion that 1.60% across the four buckets:
//   1.00% / 1.60% = 6250 bps → LP_REINJECT_BPS
//   0.40% / 1.60% = 2500 bps → REWARD_POOL_BPS  (Diamond Hands holder pool)
//   0.10% / 1.60% =  625 bps → DEV_BONUS_BPS   (Deployer Bonus, eligibility-gated)
//   0.10% / 1.60% =  625 bps → PLATFORM_LP_BPS (platform revenue)
//
// When the deployer fails the eligibility check (no initial_buy_amount captured,
// or current creator balance < initial_buy_amount), the DEV_BONUS_BPS slice is
// rolled into the holder pool for THAT cycle — boosting REWARD effectively to
// 31.25% of the wSOL share. Deployers who later qualify get the bonus on
// future cycles; past forfeited slices stay forfeited.

pub const LP_REINJECT_BPS: u64 = 6250; // 62.5% — feeds add_liquidity
pub const REWARD_POOL_BPS: u64 = 2500; // 25%   — Diamond Hands holder pool
pub const DEV_BONUS_BPS:   u64 =  625; // 6.25% — Deployer Bonus (replaced legacy NEST_EGG_BPS in Phase 3)
pub const PLATFORM_LP_BPS: u64 =  625; // 6.25% — platform revenue

/// One-shot bootstrap claim amount, in lamports = 5 SOL. Funds Dexscreener
/// listings + early launch costs without waiting for the 10-day Diamond Hands
/// eligibility window. Eligibility: creator must still hold >= initial_buy_amount.
/// USD volatility accepted for v1; future work = Pyth oracle for USD-stable cap.
pub const BOOTSTRAP_LAMPORTS: u64 = 5_000_000_000;

/// Carved out of the bonded SOL at `create_meteora_pool` time and routed to
/// the cranker that performs the migration. Reimburses the per-migration
/// out-of-pocket cost (~0.026 SOL of stranded PDA rent + Meteora pool/position
/// rent) plus a ~35% buffer so the cranker accumulates surplus over time
/// instead of slowly bleeding to zero.
///
/// Why this exists: without it, the cranker wallet only ever DRAINS. Every
/// migration costs ~0.026 SOL net (migration_authority stranded rent +
/// reward_vault + deployer_vault + Meteora's pool/position/vault rent +
/// tx fees), and there is no incoming stream to balance it. Operator has to
/// top up by hand periodically or the bot eventually OOS and migrations
/// stop landing. Carving 0.035 SOL out of the bonded raise makes the
/// cranker self-sustaining + gives it a buffer for priority fees, rent
/// rate increases, or Meteora layout changes.
///
/// 0.035 SOL on a 121 SOL raise = 0.029% of the LP depth. LP gets
/// `bonded - 0.035 SOL` instead of `bonded` — the gap funds the
/// infrastructure that ALSO maintains that LP via `claim_and_reinject`.
/// Documented in the public fee breakdown; cranker wallet pubkey is
/// public so anyone can audit where this 0.035 SOL flows.
///
/// Permissionless implications: `create_meteora_pool` accepts any caller
/// as the cranker (front-running protection sits in the M-1 sqrt-price
/// sanity gate, not in caller identity). The 0.035 SOL flows to whoever
/// successfully completes the migration. Same threat model as the
/// existing `reimburse` flow at step 9 — already permissionless, no new
/// surface area introduced.
pub const CRANKER_MIGRATION_REIMBURSEMENT_LAMPORTS: u64 = 35_000_000; // 0.035 SOL

/// Diamond Hands per-mint reward vault PDA seed.
pub const REWARD_VAULT_SEED: &[u8] = b"reward_vault";

/// Only signer permitted to call `admin_withdraw_rewards`. v1 rotation =
/// program upgrade. The legacy `admin_withdraw_nest_egg` (also gated by this
/// authority) was removed in Phase 3 of the NEST_EGG_TO_DEV_BONUS rework —
/// dev-bonus claim flows are user-signed, not admin-relayed.
///
/// Cluster-conditional: separate devnet vs mainnet keypairs. Devnet keypair
/// at ~/Desktop/mainnet-keys/rewards-authority/rewards-authority-devnet.json;
/// mainnet at ~/Desktop/mainnet-keys/rewards-authority/rewards-authority-mainnet.json.
/// `REWARDS_AUTHORITY_SECRET` env var on Vercel must match whichever cluster
/// the build targets.
#[cfg(feature = "mainnet")]
pub const REWARDS_AUTHORITY: Pubkey = pubkey!("HDJ3AmR2sWKXidz92i7XYHAWZEEXPh8F7dRSL2WFBLPF");
#[cfg(not(feature = "mainnet"))]
pub const REWARDS_AUTHORITY: Pubkey = pubkey!("FPFFavVNkhU8zp2vxaSKhSbCnRSRDiYhk2eZorbw2Nuh");

// ─── Ricochet (inline bot-platform protection) ───────────────────────────────
//
// Replaces the standalone Token-2022 TransferHook program at ~/Desktop/ricochet/.
// See docs/architecture/RICOCHET_INLINE.md for full migration rationale.
//
// Architecture: deny-by-default allowlist scan of the Instructions sysvar at
// the top of buy/sell. Any non-system program in the tx that isn't on
// RICOCHET_ALLOWED_PROGRAMS causes a revert. **Do not invert this to a
// blocklist** — that would require chasing every new bot platform; the
// allowlist defeats hidden-bottom-of-stack and unknown-program attacks by
// construction.

/// PDA seed for the per-mint Ricochet enforcement config (optional — only
/// created if the launch enables Ricochet).
pub const RICOCHET_CONFIG_SEED: &[u8] = b"ricochet_config";

/// Programs allowed to interact with a Ricochet-protected mint during the
/// enforcement window. Lifted verbatim from the deployed ricochet program's
/// DexAllowlist PDA on devnet (`8rqGN571ScW88TcpdgJ5rquo7eYYQLCERpty4hgjW1Pv`).
/// Updates require a program upgrade.
///
/// Notable absence: Meteora DAMM v2 (`cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG`)
/// is NOT here. DAMM v2 is only invoked via CPI from `migrate_to_pool` and
/// `create_meteora_pool`, never as a top-level program in a buy/sell tx, so
/// the sysvar scan never sees it.
pub const RICOCHET_ALLOWED_PROGRAMS: &[Pubkey] = &[
    pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"), // Jupiter v6 Aggregator
    pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"), // Raydium AMM v4
    pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"), // Raydium CPMM
    pubkey!("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj"), // Raydium LaunchLab
    pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"), // Orca Whirlpool
    pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"), // Meteora DLMM
    pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"), // Phoenix DEX
    pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"), // Pump.fun AMM
];

/// System programs auto-exempt from the allowlist scan. Same set the original
/// hook ignored. Includes our own program so direct user calls to buy/sell
/// (where the top-level program_id IS launchctrl) pass through cleanly.
///
/// The final entry (our own program ID) is cluster-conditional and must match
/// whichever `declare_id!` branch is active in lib.rs. Anything else here is
/// canonical across clusters.
pub const RICOCHET_EXEMPT_PROGRAMS: &[Pubkey] = &[
    pubkey!("11111111111111111111111111111111"),               // System program
    pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),    // SPL Token
    pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"),    // Token-2022
    pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),    // Associated Token Account
    pubkey!("ComputeBudget111111111111111111111111111111"),     // Compute Budget
    // ── Wallet-injected safety / utility infrastructure (added 2026-05-22) ───
    // Audited / passive programs that wallets wrap user txs with. None of
    // these can move funds — Lighthouse only asserts state, Memo only attaches
    // text, ALT only manages lookup tables. Blocking them was breaking buys
    // and sells from Phantom + Solflare. Source: RICOCHET_ALLOWLIST_RESEARCH.md
    // on operator's Desktop.
    pubkey!("L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95"),    // Lighthouse — pre/post-tx safety assertions
    pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"),    // SPL Memo v2
    pubkey!("Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo"),    // SPL Memo v1 (legacy)
    pubkey!("AddressLookupTab1e1111111111111111111111111"),    // Address Lookup Table
    // Metaplex Token Metadata (added 2026-05-22). Our own launch flow injects
    // a CreateV1 ix between mintTo and setAuthority so external tools (Phantom,
    // Solscan, Jupiter) can read token name/symbol. When initial-buy is also
    // enabled and KOL Shield is NOT, the Buy ix lands in the same tx and the
    // Ricochet scan would see CreateV1 and revert. Standard ecosystem program,
    // can't move user funds — only manages metadata accounts. Same category
    // as Token-2022 / ATA / System above.
    pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s"),    // Metaplex Token Metadata
    #[cfg(feature = "mainnet")]
    pubkey!("CTRLY9aJ4eSnFU1W3S9QUVoHhMme7T5fR8gMCpkTFuWe"),    // launchctrl mainnet
    #[cfg(not(feature = "mainnet"))]
    pubkey!("EJTstPiwyJ7a9wMUKrBDf19GLwqGwD7H2BXLFD1v1rAo"),    // launchctrl devnet
];
