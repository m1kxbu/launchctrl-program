use anchor_lang::prelude::*;
use anchor_spl::token_interface::spl_token_2022;
use anchor_spl::token_interface::{Mint, TokenAccount};

use crate::constants::*;
use crate::errors::LaunchCtrlError;
use crate::events::BootstrapClaimed;
use crate::state::{DeployerVault, LaunchState};

/// One-shot 5 SOL bootstrap claim from `deployer_vault` to the launch creator.
///
/// Designed to fund early-launch costs (Dexscreener listing, basic marketing)
/// before the LP flywheel has accrued enough to make the per-cycle dev bonus
/// meaningful. Pre-conditions, all enforced on-chain:
///
///   1. Caller is `LaunchState.creator` (Anchor `has_one = creator` + `Signer`).
///   2. Creator made an initial buy at launch (`initial_buy_amount > 0`).
///   3. Bootstrap hasn't been claimed yet (`!bootstrap_claimed`).
///   4. Creator still holds at least their initial-buy amount in their ATA.
///   5. `deployer_vault` has accumulated at least 5 SOL above its rent-exempt
///      minimum (otherwise the LP flywheel hasn't earned enough yet).
///
/// All five must be true at tx-include-time. The "still holding" check uses
/// the same on-chain gate as `claim_and_reinject`'s eligibility branch — a
/// stale-state flash-borrow attack is theoretically possible but bounded at
/// 5 SOL with a -90%+ ROI for the attacker (per the threat model in the
/// implementation plan). v2 hardening can move to the off-chain Diamond Hands
/// snapshot pipeline if exploited in practice.
///
/// No CPIs, so post-check raw lamport mutation on the program-owned vault
/// is safe under Solana's lamport-conservation rule.
pub fn claim_bootstrap(ctx: Context<ClaimBootstrap>) -> Result<()> {
    let launch = &mut ctx.accounts.launch_state;

    // ── 1. Eligibility gates ────────────────────────────────────────────────
    require!(launch.initial_buy_amount > 0, LaunchCtrlError::NoInitialBuy);
    require!(
        !launch.bootstrap_claimed,
        LaunchCtrlError::BootstrapAlreadyClaimed
    );
    require!(
        ctx.accounts.creator_token_account.amount >= launch.initial_buy_amount,
        LaunchCtrlError::NotHoldingInitialBuy
    );

    // ── 2. Vault balance check ──────────────────────────────────────────────
    // DeployerVault has space=8 (just the discriminator). Leave at least the
    // rent-exempt minimum behind to keep the PDA alive — the same vault keeps
    // accumulating fees post-bootstrap, just for the per-cycle dev bonus.
    let vault_info = ctx.accounts.deployer_vault.to_account_info();
    let creator_info = ctx.accounts.creator.to_account_info();
    let rent_min = Rent::get()?.minimum_balance(8);
    let current = vault_info.lamports();
    let required = rent_min
        .checked_add(BOOTSTRAP_LAMPORTS)
        .ok_or(LaunchCtrlError::MathOverflow)?;
    require!(current >= required, LaunchCtrlError::BootstrapNotReady);

    // ── 3. Flip the claimed flag BEFORE the lamport move ────────────────────
    // Solana commits the whole tx or none, so ordering inside a non-CPI
    // handler can't be exploited via reentrancy. Setting the flag first is
    // pure hygiene — makes the post-condition obvious in static reads.
    launch.bootstrap_claimed = true;

    // ── 4. Raw-debit 5 SOL: deployer_vault → creator ────────────────────────
    **vault_info.try_borrow_mut_lamports()? = current
        .checked_sub(BOOTSTRAP_LAMPORTS)
        .ok_or(LaunchCtrlError::InsufficientDeployerVaultBalance)?;
    **creator_info.try_borrow_mut_lamports()? = creator_info
        .lamports()
        .checked_add(BOOTSTRAP_LAMPORTS)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    emit!(BootstrapClaimed {
        mint: ctx.accounts.mint.key(),
        creator: ctx.accounts.creator.key(),
        amount_lamports: BOOTSTRAP_LAMPORTS,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct ClaimBootstrap<'info> {
    /// Launch creator — must equal `LaunchState.creator` (cross-checked via
    /// the `has_one = creator` constraint on `launch_state` below).
    #[account(mut)]
    pub creator: Signer<'info>,

    #[account(
        constraint = mint.to_account_info().owner == &spl_token_2022::ID
            @ LaunchCtrlError::InvalidMintOwner
    )]
    pub mint: InterfaceAccount<'info, Mint>,

    /// Boxed for stack safety (LaunchState is 525 bytes). `bootstrap_claimed`
    /// flips to true inside the handler, so this is `mut`.
    #[account(
        mut,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_state.bump,
        has_one = mint,
        has_one = creator,
    )]
    pub launch_state: Box<Account<'info, LaunchState>>,

    /// Creator's ATA for this mint. Used to read the current balance for the
    /// still-holding check. Constrained to:
    ///   - `mint == this mint` (cross-mint substitution blocked)
    ///   - `authority == creator` (cross-wallet substitution blocked, and
    ///     the `has_one = creator` on launch_state binds creator → launch).
    #[account(
        token::mint = mint,
        token::authority = creator,
    )]
    pub creator_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// Per-mint Deployer Bonus accumulator. Raw lamport-debited by this ix.
    /// Same PDA as the one used by `claim_and_reinject`; this just spends
    /// from it once.
    #[account(
        mut,
        seeds = [DEPLOYER_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub deployer_vault: Account<'info, DeployerVault>,
}
