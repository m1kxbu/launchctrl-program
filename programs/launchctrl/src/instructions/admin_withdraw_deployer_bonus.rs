use anchor_lang::prelude::*;
use anchor_spl::token_interface::spl_token_2022;
use anchor_spl::token_interface::{Mint, TokenAccount};

use crate::constants::*;
use crate::errors::LaunchCtrlError;
use crate::events::DeployerBonusWithdrawn;
use crate::state::{DeployerVault, LaunchState};

/// Permissioned withdrawal from a mint's per-mint `deployer_vault` to the
/// launch creator. Mirror of `admin_withdraw_rewards` for the deployer-bonus
/// stream — same `REWARDS_AUTHORITY` signer, same off-chain Diamond Hands
/// 10-day eligibility check in the API, but adds an on-chain still-holding
/// gate so a creator who dumped below their initial buy can never extract
/// from this vault even if the off-chain check is bypassed.
///
/// Typical flow per cycle (when wallet == launch.creator + Diamond Hands
/// eligible + still holding initial):
///   1. The standard `/api/staking/[mint]/claim` route signs both
///      `admin_withdraw_rewards` AND this instruction in a single tx.
///   2. The creator gets their normal Diamond Hands share from `reward_vault`
///      PLUS whatever has accumulated in `deployer_vault` since the last
///      claim.
///   3. Both succeed atomically or both revert.
///
/// The 5 SOL bootstrap (`claim_bootstrap`) is a separate one-shot path
/// designed for early-launch costs. After bootstrap, post-cycle accumulation
/// is extracted via THIS instruction on the regular 10-day cadence.
///
/// No CPIs, so post-check raw lamport mutation on the program-owned vault
/// is safe under the lamport-conservation rule.
pub fn admin_withdraw_deployer_bonus(
    ctx: Context<AdminWithdrawDeployerBonus>,
    amount: u64,
) -> Result<()> {
    require!(amount > 0, LaunchCtrlError::ZeroAmount);

    // ── On-chain still-holding gate ─────────────────────────────────────────
    // Defense-in-depth: the API enforces Diamond Hands eligibility off-chain
    // (10-day non-decreasing balance), but this on-chain check ensures that
    // even a buggy or compromised API can't drain the deployer vault for a
    // creator who dumped below their initial buy. Same gate semantics as
    // `claim_bootstrap` and the eligibility branch in `claim_and_reinject`.
    require!(
        ctx.accounts.launch_state.initial_buy_amount > 0,
        LaunchCtrlError::NoInitialBuy
    );
    require!(
        ctx.accounts.creator_token_account.amount >= ctx.accounts.launch_state.initial_buy_amount,
        LaunchCtrlError::NotHoldingInitialBuy
    );

    // ── Vault balance check ─────────────────────────────────────────────────
    // DeployerVault has space=8 (just the discriminator). Leave the rent-
    // exempt minimum behind so the per-cycle accumulation can keep flowing
    // into this same PDA — it's the same vault `claim_and_reinject` deposits
    // into every cycle.
    let vault_info = ctx.accounts.deployer_vault.to_account_info();
    let creator_info = ctx.accounts.creator.to_account_info();
    let rent_min = Rent::get()?.minimum_balance(8);
    let current = vault_info.lamports();
    let available = current.saturating_sub(rent_min);
    require!(
        amount <= available,
        LaunchCtrlError::InsufficientDeployerVaultBalance
    );

    // ── Raw-debit: deployer_vault → creator ─────────────────────────────────
    **vault_info.try_borrow_mut_lamports()? = current
        .checked_sub(amount)
        .ok_or(LaunchCtrlError::InsufficientDeployerVaultBalance)?;
    **creator_info.try_borrow_mut_lamports()? = creator_info
        .lamports()
        .checked_add(amount)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    emit!(DeployerBonusWithdrawn {
        mint: ctx.accounts.mint.key(),
        creator: ctx.accounts.creator.key(),
        amount_lamports: amount,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct AdminWithdrawDeployerBonus<'info> {
    /// Signer must match the hardcoded `REWARDS_AUTHORITY` constant — same
    /// authority that signs `admin_withdraw_rewards`. Rotation = program
    /// upgrade. v2 will replace both with a per-mint config or Merkle-root
    /// claim.
    #[account(
        constraint = rewards_authority.key() == REWARDS_AUTHORITY
            @ LaunchCtrlError::UnauthorizedRewardsAuthority
    )]
    pub rewards_authority: Signer<'info>,

    #[account(
        constraint = mint.to_account_info().owner == &spl_token_2022::ID
            @ LaunchCtrlError::InvalidMintOwner
    )]
    pub mint: InterfaceAccount<'info, Mint>,

    /// Boxed for stack safety (LaunchState is 525 bytes). Read-only — this
    /// instruction doesn't mutate launch_state. `has_one = creator` cross-
    /// binds the recipient below to the launch's recorded creator, blocking
    /// recipient-substitution attacks even if a compromised API tries to
    /// pay a different wallet.
    #[account(
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_state.bump,
        has_one = mint,
        has_one = creator,
    )]
    pub launch_state: Box<Account<'info, LaunchState>>,

    /// CHECK: SOL recipient. Bound to `launch_state.creator` via the
    /// `has_one = creator` constraint above. The instruction never signs
    /// with this account — it's just credited lamports.
    #[account(mut)]
    pub creator: UncheckedAccount<'info>,

    /// Creator's Token-2022 ATA. Used to read the current balance for the
    /// still-holding gate. Anchor enforces:
    ///   - `mint == this mint` (cross-mint substitution blocked)
    ///   - `authority == creator` (cross-wallet substitution blocked)
    /// Same constraint shape as `claim_and_reinject` and `claim_bootstrap`.
    #[account(
        token::mint = mint,
        token::authority = creator,
    )]
    pub creator_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// Per-mint deployer-bonus accumulator. Raw lamport-debited here.
    #[account(
        mut,
        seeds = [DEPLOYER_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub deployer_vault: Account<'info, DeployerVault>,
}
