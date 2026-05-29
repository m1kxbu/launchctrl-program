use anchor_lang::prelude::*;
use solana_instructions_sysvar::load_instruction_at_checked;

use crate::constants::{RICOCHET_ALLOWED_PROGRAMS, RICOCHET_CONFIG_SEED, RICOCHET_EXEMPT_PROGRAMS};
use crate::errors::LaunchCtrlError;
use crate::state::RicochetConfig;

/// Inline replacement for the standalone Token-2022 TransferHook program at
/// ~/Desktop/ricochet/. Same allowlist scan, same Instructions sysvar source,
/// same revert outcome — relocated from `transfer_checked` to the top of
/// `buy` / `sell` handlers.
///
/// **Allowlist, not blocklist.** Any non-exempt program in the tx that isn't
/// on `RICOCHET_ALLOWED_PROGRAMS` causes a revert. This is the original
/// Ricochet design's defining choice — see docs/architecture/RICOCHET_INLINE.md decision #2a
/// for the security reasoning.
///
/// **Curve-complete short-circuit (added 2026-05-22).** Ricochet protects the
/// *bonding-curve* phase against bot platforms. Once the curve fills and the
/// crank migrates to Meteora, post-migration trades flow through Jupiter and
/// don't even reach this code. But the 0–2 min window between curve-fill and
/// the bot crank's `migrate_to_pool` run is still on-curve sell territory,
/// and during that window enforce_ricochet was still scanning — for no good
/// reason, since the protection job is effectively done. Short-circuit on
/// `curve_is_complete` so that window stops generating spurious reverts on
/// wallets that wrap with non-allowlisted helper programs.
pub fn enforce_ricochet<'info>(
    config: &Option<Account<'info, RicochetConfig>>,
    ix_sysvar: &AccountInfo<'info>,
    mint: &Pubkey,
    curve_is_complete: bool,
) -> Result<()> {
    let Some(cfg) = config else {
        return Ok(());
    };

    if curve_is_complete {
        return Ok(());
    }

    // Verify the account is the canonical PDA for this mint. Done here instead
    // of via Anchor's `seeds = [..]` constraint because `Option<Account<...>>`
    // can't self-reference for `bump`. Use `find_program_address` (not
    // `create_program_address`) so the canonical bump is enforced regardless
    // of what's stored on the account — defends against seed-canonicalization
    // attacks even if a future code path were to create a `RicochetConfig`
    // with a non-canonical bump.
    let (expected_pda, expected_bump) =
        Pubkey::find_program_address(&[RICOCHET_CONFIG_SEED, mint.as_ref()], &crate::ID);
    require_keys_eq!(cfg.key(), expected_pda, LaunchCtrlError::RicochetMintMismatch);
    require!(cfg.bump == expected_bump, LaunchCtrlError::RicochetMintMismatch);
    require_keys_eq!(cfg.mint, *mint, LaunchCtrlError::RicochetMintMismatch);

    let now = Clock::get()?.unix_timestamp;
    if now >= cfg.expires_at {
        return Ok(());
    }

    scan_for_blocked_programs(ix_sysvar)
}

/// Scan the Instructions sysvar for any non-exempt, non-allowlisted program.
/// Identical logic to the original `ricochet::execute` hook.
fn scan_for_blocked_programs(ix_sysvar: &AccountInfo) -> Result<()> {
    let mut idx: usize = 0;
    loop {
        match load_instruction_at_checked(idx, ix_sysvar) {
            Ok(ix) => {
                if !is_exempt(&ix.program_id) && !is_allowed(&ix.program_id) {
                    msg!("Ricochet: ix[{}] = {} — not on allowlist — BLOCKING", idx, ix.program_id);
                    return Err(LaunchCtrlError::UnauthorizedPlatform.into());
                }
                idx += 1;
            }
            Err(_) => break, // end of instructions
        }
    }
    Ok(())
}

#[inline]
fn is_exempt(program: &Pubkey) -> bool {
    RICOCHET_EXEMPT_PROGRAMS.iter().any(|p| p == program)
}

#[inline]
fn is_allowed(program: &Pubkey) -> bool {
    RICOCHET_ALLOWED_PROGRAMS.iter().any(|p| p == program)
}
