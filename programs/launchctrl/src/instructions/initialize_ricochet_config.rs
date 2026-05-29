use anchor_lang::prelude::*;
use anchor_spl::token_interface::Mint;

use crate::constants::RICOCHET_CONFIG_SEED;
use crate::errors::LaunchCtrlError;
use crate::events::RicochetConfigInitialized;
use crate::state::{LaunchState, RicochetConfig};

/// Sanity cap on `duration_seconds`. The longest legitimate Ricochet duration
/// in the frontend is `"decay"` (sum of decay schedule) which won't exceed a
/// few hours in practice. 86_400 (24h) gives plenty of headroom and protects
/// against typo'd inputs that effectively persist enforcement past migration.
/// Buy/sell aren't called post-migration anyway, so a too-large value is
/// functionally harmless — but a hard cap is cheap defense.
const MAX_DURATION_SECONDS: u32 = 86_400;

/// Optional per-mint Ricochet enablement. Replaces the standalone
/// ricochet program's `initialize_mint_enforce` + `initialize_extra_account_meta_list`
/// pair (the old TX1b in the launch flow).
///
/// Caller must be the launch creator (matches `LaunchState.creator`).
/// Computes `expires_at = launch_timestamp + duration_seconds` so enforcement
/// is checked stateless against `Clock::get()` in `buy.rs` / `sell.rs` — no
/// off-chain crank needed.
pub fn initialize_ricochet_config(
    ctx: Context<InitializeRicochetConfig>,
    duration_seconds: u32,
) -> Result<()> {
    require!(
        duration_seconds > 0 && duration_seconds <= MAX_DURATION_SECONDS,
        LaunchCtrlError::RicochetDurationOutOfRange,
    );

    let launch = &ctx.accounts.launch_state;
    let expires_at = launch
        .launch_timestamp
        .checked_add(duration_seconds as i64)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    let cfg = &mut ctx.accounts.ricochet_config;
    cfg.mint = ctx.accounts.mint.key();
    cfg.expires_at = expires_at;
    cfg.bump = ctx.bumps.ricochet_config;

    emit!(RicochetConfigInitialized {
        mint: ctx.accounts.mint.key(),
        expires_at,
        duration_seconds,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct InitializeRicochetConfig<'info> {
    /// Launch creator — must match `LaunchState.creator`.
    #[account(mut)]
    pub creator: Signer<'info>,

    pub mint: InterfaceAccount<'info, Mint>,

    /// Existing launch state — must be created (gives us `launch_timestamp`)
    /// and the creator must match the signer.
    #[account(
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_state.bump,
        constraint = launch_state.creator == creator.key() @ LaunchCtrlError::Unauthorized,
        constraint = launch_state.mint == mint.key(),
    )]
    pub launch_state: Account<'info, LaunchState>,

    /// New per-mint Ricochet config PDA. Init only — cannot be re-initialized
    /// (fixed enforcement window per launch by design).
    #[account(
        init,
        payer = creator,
        space = RicochetConfig::LEN,
        seeds = [RICOCHET_CONFIG_SEED, mint.key().as_ref()],
        bump,
    )]
    pub ricochet_config: Account<'info, RicochetConfig>,

    pub system_program: Program<'info, System>,
}
