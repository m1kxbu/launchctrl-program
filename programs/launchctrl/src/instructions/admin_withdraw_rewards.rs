use anchor_lang::prelude::*;

use crate::constants::*;
use crate::errors::LaunchCtrlError;
use crate::events::RewardsWithdrawn;
use crate::state::RewardVault;

/// Permissioned withdrawal from a mint's Diamond Hands reward_vault.
///
/// v1 design: rewards accounting lives off-chain (Supabase). The backend
/// verifies eligibility and multiplier, then signs this instruction with the
/// `REWARDS_AUTHORITY` key to move SOL out of the program-owned reward_vault
/// to the eligible holder. v2 will replace this with a user-signed claim
/// against an on-chain Merkle root or equivalent.
///
/// Only the hardcoded `REWARDS_AUTHORITY` pubkey can call this. Rotation =
/// program upgrade.
///
/// No CPIs, so post-check raw lamport mutation on the program-owned
/// `reward_vault` is safe.
pub fn admin_withdraw_rewards(
    ctx: Context<AdminWithdrawRewards>,
    amount: u64,
) -> Result<()> {
    require!(amount > 0, LaunchCtrlError::ZeroAmount);

    let vault_info = ctx.accounts.reward_vault.to_account_info();
    let recipient_info = ctx.accounts.recipient.to_account_info();

    // The vault is a zero-data program-owned account. We must leave enough
    // lamports behind to keep it rent-exempt — otherwise the account can be
    // reaped and the PDA has to be re-initialized.
    let rent_min = Rent::get()?.minimum_balance(8);
    let current = vault_info.lamports();
    let available = current.saturating_sub(rent_min);
    require!(
        amount <= available,
        LaunchCtrlError::InsufficientRewardVaultBalance
    );

    **vault_info.try_borrow_mut_lamports()? = current
        .checked_sub(amount)
        .ok_or(LaunchCtrlError::InsufficientRewardVaultBalance)?;
    **recipient_info.try_borrow_mut_lamports()? = recipient_info
        .lamports()
        .checked_add(amount)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    emit!(RewardsWithdrawn {
        mint: ctx.accounts.mint.key(),
        recipient: ctx.accounts.recipient.key(),
        amount_lamports: amount,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct AdminWithdrawRewards<'info> {
    /// Signer must match the hardcoded `REWARDS_AUTHORITY` constant.
    #[account(
        constraint = rewards_authority.key() == REWARDS_AUTHORITY
            @ LaunchCtrlError::UnauthorizedRewardsAuthority
    )]
    pub rewards_authority: Signer<'info>,

    /// CHECK: Used only to derive the `reward_vault` PDA seed. Not validated
    /// as a real Token-2022 mint here; the invariant is "reward_vault exists
    /// at `[REWARD_VAULT_SEED, mint]`", which is enforced by the seeds below.
    pub mint: UncheckedAccount<'info>,

    /// Per-mint rewards accumulator. Raw lamport-debited by this instruction.
    #[account(
        mut,
        seeds = [REWARD_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub reward_vault: Account<'info, RewardVault>,

    /// CHECK: SOL recipient. The backend is responsible for passing the
    /// eligible holder's wallet here; on-chain has no per-user accounting.
    #[account(mut)]
    pub recipient: UncheckedAccount<'info>,
}
