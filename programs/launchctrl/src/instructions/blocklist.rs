use anchor_lang::prelude::*;
use anchor_spl::token_interface::Mint;

use crate::constants::MAX_BLOCKLIST_SIZE;
use crate::errors::LaunchCtrlError;
use crate::state::*;

/// Add wallet addresses to the per-token blocklist.
/// Creator only, pre-first-buy. The blocklist freezes the moment the first
/// buy lands (`buy.rs` flips `frozen = true`); after that this ix reverts
/// with `BlocklistFrozen`. Batches up to 50 addresses per call.
pub fn add_to_blocklist(ctx: Context<AddToBlocklist>, addresses: Vec<Pubkey>) -> Result<()> {
    require!(!addresses.is_empty(), LaunchCtrlError::ZeroAmount);
    let blocklist = &mut ctx.accounts.blocklist;
    for addr in &addresses {
        require!(
            blocklist.blocked.len() < MAX_BLOCKLIST_SIZE,
            LaunchCtrlError::BlocklistFull
        );
        if !blocklist.blocked.contains(addr) {
            blocklist.blocked.push(*addr);
        }
    }
    Ok(())
}

/// Remove wallet addresses from the per-token blocklist.
/// Creator only, pre-first-buy. Same freeze semantics as add_to_blocklist.
pub fn remove_from_blocklist(
    ctx: Context<RemoveFromBlocklist>,
    addresses: Vec<Pubkey>,
) -> Result<()> {
    let blocklist = &mut ctx.accounts.blocklist;
    blocklist.blocked.retain(|b| !addresses.contains(b));
    Ok(())
}

#[derive(Accounts)]
pub struct AddToBlocklist<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,

    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        seeds = [b"blocklist", mint.key().as_ref()],
        bump = blocklist.bump,
        constraint = blocklist.creator == creator.key() @ LaunchCtrlError::Unauthorized,
        constraint = !blocklist.frozen @ LaunchCtrlError::BlocklistFrozen,
    )]
    pub blocklist: Account<'info, Blocklist>,

    /// Prevent modifications after migration.
    #[account(
        seeds = [b"curve", mint.key().as_ref()],
        bump = curve_state.bump,
        constraint = !curve_state.is_complete @ LaunchCtrlError::CurveComplete,
    )]
    pub curve_state: Account<'info, CurveState>,
}

#[derive(Accounts)]
pub struct RemoveFromBlocklist<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,

    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        seeds = [b"blocklist", mint.key().as_ref()],
        bump = blocklist.bump,
        constraint = blocklist.creator == creator.key() @ LaunchCtrlError::Unauthorized,
        constraint = !blocklist.frozen @ LaunchCtrlError::BlocklistFrozen,
    )]
    pub blocklist: Account<'info, Blocklist>,

    #[account(
        seeds = [b"curve", mint.key().as_ref()],
        bump = curve_state.bump,
        constraint = !curve_state.is_complete @ LaunchCtrlError::CurveComplete,
    )]
    pub curve_state: Account<'info, CurveState>,
}
