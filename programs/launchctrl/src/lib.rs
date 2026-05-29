use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod instructions;
pub mod math;
pub mod state;

// The #[program] macro resolves Accounts structs and their auto-generated
// companion __client_accounts_* modules at crate root, so we glob re-export
// everything from instructions::*. This produces benign "ambiguous glob"
// warnings for the handler function names — the macro-generated handlers
// shadow the imported ones, which is the desired behavior. Silenced with
// the explicit allow below since the shadowing is intentional.
#[allow(ambiguous_glob_reexports)]
pub use errors::*;
#[allow(ambiguous_glob_reexports)]
pub use events::*;
#[allow(ambiguous_glob_reexports)]
pub use instructions::*;
#[allow(ambiguous_glob_reexports)]
pub use state::*;

// Cluster-conditional program ID. Default (devnet) build keeps the devnet
// address so existing tests + bot don't break. `--features mainnet` swaps
// in the CTRL-vanity mainnet address. The constants.rs RICOCHET_EXEMPT_PROGRAMS
// self-entry must stay in sync with whichever branch is active here.
#[cfg(feature = "mainnet")]
declare_id!("CTRLY9aJ4eSnFU1W3S9QUVoHhMme7T5fR8gMCpkTFuWe");
#[cfg(not(feature = "mainnet"))]
declare_id!("EJTstPiwyJ7a9wMUKrBDf19GLwqGwD7H2BXLFD1v1rAo");

// security.txt — Neodyme Riverguard ownership-verification anchor plus the
// industry-standard way to surface a security contact embedded in the
// program binary. Read by `riverguard.io`, Solana audit firms, and any
// security researcher who runs `solana program dump` + `strings` on the
// .so. NOT a runtime check — the macro emits a `.security.txt` section
// in the ELF that's invisible at execution time.
//
// Spec: https://github.com/neodyme-labs/solana-security-txt
#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "LaunchCTRL",
    project_url: "https://inctrl.fun",
    contacts: "email:security@inctrl.fun,twitter:@shiftinctrl",
    policy: "https://inctrl.fun/docs",
    preferred_languages: "en",
    auditors: "Sec3 X-ray (2026-04-26) + internal manual review + ongoing"
}

#[program]
pub mod launchctrl {
    use super::*;

    // ── Launch lifecycle ────────────────────────────────────────────────────
    pub fn initialize_launch(
        ctx: Context<InitializeLaunch>,
        params: LaunchParams,
    ) -> Result<()> {
        instructions::initialize::initialize_launch(ctx, params)
    }

    pub fn initialize_curve(
        ctx: Context<InitializeCurve>,
        migration_threshold_lamports: u64,
    ) -> Result<()> {
        instructions::initialize::initialize_curve(ctx, migration_threshold_lamports)
    }

    // ── Trading ─────────────────────────────────────────────────────────────
    pub fn buy(ctx: Context<Buy>, sol_amount: u64, min_tokens_out: u64) -> Result<()> {
        instructions::buy::buy(ctx, sol_amount, min_tokens_out)
    }

    pub fn sell(ctx: Context<Sell>, token_amount: u64, min_sol_out: u64) -> Result<()> {
        instructions::sell::sell(ctx, token_amount, min_sol_out)
    }

    // ── KOL Shield / blocklist ──────────────────────────────────────────────
    pub fn add_to_blocklist(
        ctx: Context<AddToBlocklist>,
        addresses: Vec<Pubkey>,
    ) -> Result<()> {
        instructions::blocklist::add_to_blocklist(ctx, addresses)
    }

    pub fn remove_from_blocklist(
        ctx: Context<RemoveFromBlocklist>,
        addresses: Vec<Pubkey>,
    ) -> Result<()> {
        instructions::blocklist::remove_from_blocklist(ctx, addresses)
    }

    // ── Migration ───────────────────────────────────────────────────────────
    pub fn migrate_to_pool(ctx: Context<MigrateToPool>) -> Result<()> {
        instructions::migrate::migrate_to_pool(ctx)
    }

    pub fn create_meteora_pool(
        ctx: Context<CreateMeteoraPool>,
        init_sqrt_price: u128,
        liquidity_delta: u128,
        drip_total: u64,
    ) -> Result<()> {
        instructions::create_pool::create_meteora_pool(ctx, init_sqrt_price, liquidity_delta, drip_total)
    }

    /// LP flywheel crank — claim Meteora LP fees, run the 4-way split
    /// (62.5% reinject / 25% rewards / 6.25% deployer-bonus / 6.25% platform),
    /// re-deposit the 62.5% via add_liquidity. The deployer-bonus slice rolls
    /// into the holder pool when the creator no longer holds their initial buy.
    /// Permissionless. NEST_EGG_TO_DEV_BONUS Phase 3.
    pub fn claim_and_reinject(
        ctx: Context<ClaimAndReinject>,
        liquidity_delta: u128,
    ) -> Result<()> {
        instructions::reinject::claim_and_reinject(ctx, liquidity_delta)
    }

    // ── Diamond Hands rewards ───────────────────────────────────────────────
    /// Permissioned withdrawal from a mint's reward_vault. v1: backend signs
    /// with `REWARDS_AUTHORITY` after off-chain eligibility check.
    pub fn admin_withdraw_rewards(
        ctx: Context<AdminWithdrawRewards>,
        amount: u64,
    ) -> Result<()> {
        instructions::admin_withdraw_rewards::admin_withdraw_rewards(ctx, amount)
    }

    // ── Deployer Bonus / Bootstrap ──────────────────────────────────────────
    /// One-shot 5 SOL bootstrap claim. Signer = launch creator. Gated on
    /// `initial_buy_amount > 0`, `!bootstrap_claimed`, creator still holding
    /// at least their initial buy, and `deployer_vault` ≥ rent + 5 SOL.
    /// NEST_EGG_TO_DEV_BONUS Phase 4.
    pub fn claim_bootstrap(ctx: Context<ClaimBootstrap>) -> Result<()> {
        instructions::claim_bootstrap::claim_bootstrap(ctx)
    }

    /// Permissioned drain of `deployer_vault` to the launch creator. Mirror
    /// of `admin_withdraw_rewards` — same `REWARDS_AUTHORITY` signer, same
    /// off-chain Diamond Hands eligibility model — but adds an on-chain
    /// still-holding gate so a bailed creator can never extract from this
    /// vault. Bundled with `admin_withdraw_rewards` in a single tx by the
    /// claim API when the claimer is the launch creator. NEST_EGG_TO_DEV_BONUS
    /// Phase 7 (recurring deployer-bonus claim).
    pub fn admin_withdraw_deployer_bonus(
        ctx: Context<AdminWithdrawDeployerBonus>,
        amount: u64,
    ) -> Result<()> {
        instructions::admin_withdraw_deployer_bonus::admin_withdraw_deployer_bonus(ctx, amount)
    }

    // ── Ricochet (inline bot-platform protection) ──────────────────────────
    /// Optional per-mint Ricochet enablement. Replaces the standalone ricochet
    /// program's TX1b (`initialize_mint_enforce` + `initialize_extra_account_meta_list`).
    /// See docs/architecture/RICOCHET_INLINE.md for the migration plan.
    pub fn initialize_ricochet_config(
        ctx: Context<InitializeRicochetConfig>,
        duration_seconds: u32,
    ) -> Result<()> {
        instructions::initialize_ricochet_config::initialize_ricochet_config(ctx, duration_seconds)
    }
}
