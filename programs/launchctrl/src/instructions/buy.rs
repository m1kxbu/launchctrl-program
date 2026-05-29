use anchor_lang::prelude::*;
use anchor_spl::token_interface::spl_token_2022;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{Mint, TokenAccount};

use solana_instructions_sysvar::ID as INSTRUCTIONS_SYSVAR_ID;

use crate::constants::*;
use crate::errors::LaunchCtrlError;
use crate::events::*;
use crate::instructions::ricochet_check::enforce_ricochet;
use crate::state::*;

/// Buy tokens with SOL.
///
/// FEE_REWORK (April 2026): 1% flat platform fee on the SOL input. No
/// Token-2022 transfer fee — vanilla mint with no extensions, so the
/// vault→buyer transfer is 1:1 (no gross-up).
pub fn buy(ctx: Context<Buy>, sol_amount: u64, min_tokens_out: u64) -> Result<()> {
    require!(!ctx.accounts.curve_state.is_complete, LaunchCtrlError::CurveComplete);
    require!(sol_amount > 0, LaunchCtrlError::ZeroAmount);
    require!(
        !ctx.accounts.blocklist.blocked.contains(&ctx.accounts.buyer.key()),
        LaunchCtrlError::Blocked
    );

    enforce_ricochet(
        &ctx.accounts.ricochet_config,
        &ctx.accounts.instructions_sysvar,
        &ctx.accounts.mint.key(),
        ctx.accounts.curve_state.is_complete,
    )?;

    // BundleGuard: tiered per-slot buy limit.
    let current_slot = Clock::get()?.slot;
    {
        let slots_elapsed = current_slot
            .saturating_sub(ctx.accounts.curve_state.creation_slot);
        let max_this_slot: u8 = if slots_elapsed <= BUNDLE_GUARD_STRICT_SLOTS {
            BUNDLE_GUARD_STRICT_MAX
        } else if slots_elapsed <= BUNDLE_GUARD_SOFT_SLOTS {
            BUNDLE_GUARD_SOFT_MAX
        } else {
            u8::MAX
        };
        if ctx.accounts.curve_state.last_buy_slot == current_slot {
            require!(
                ctx.accounts.curve_state.buys_this_slot < max_this_slot,
                LaunchCtrlError::SlotBuyLimitReached
            );
        }
    }

    // ── Fee math + constant-product quote ───────────────────────────────────
    let (platform_fee, sol_in) = crate::math::apply_buy_fee(sol_amount, BASE_FEE_BPS)
        .ok_or(LaunchCtrlError::MathOverflow)?;
    let curve = &ctx.accounts.curve_state;
    let tokens_out = crate::math::quote_buy(
        curve.virtual_sol_reserves,
        curve.real_sol_reserves,
        curve.virtual_token_reserves,
        curve.real_token_reserves,
        sol_in,
    )
    .ok_or(LaunchCtrlError::MathOverflow)?;

    require!(tokens_out >= min_tokens_out, LaunchCtrlError::SlippageExceeded);
    require!(
        tokens_out <= ctx.accounts.curve_token_vault.amount,
        LaunchCtrlError::InsufficientLiquidity
    );

    // Cap creator's initial buy at 2% of supply (only on first buy).
    if ctx.accounts.buyer.key() == ctx.accounts.curve_state.creator
        && ctx.accounts.curve_state.real_token_reserves == 0
    {
        let max_creator_tokens = VIRTUAL_TOKEN_RESERVES
            .checked_mul(MAX_CREATOR_INITIAL_BUY_BPS)
            .and_then(|v| v.checked_div(10_000))
            .ok_or(LaunchCtrlError::MathOverflow)?;
        require!(tokens_out <= max_creator_tokens, LaunchCtrlError::CreatorBuyCapExceeded);
    }

    // Capture the creator's initial-buy amount on the very first buy of the
    // curve. Gates:
    //   1. launch_state.initial_buy_amount == 0  — one-shot, never overwritten.
    //   2. curve_state.real_sol_reserves == 0    — must be the FIRST buy of
    //      the curve, before anyone else has moved the price. Closes the
    //      gameability hole where a creator skips their initial buy, lets
    //      another buyer push the curve, then buys cheap to set a small
    //      `initial_buy_amount` and unlock the bonus.
    //   3. buyer == launch_state.creator         — only the creator's buy
    //      triggers capture; another first-buyer doesn't qualify the dev for
    //      the bonus.
    // If any gate fails, capture silently does not occur. A creator who skips
    // the launch-tx initial buy forfeits the deployer bonus and bootstrap
    // forever — surfaced in the launch form copy as a hard rule.
    if ctx.accounts.launch_state.initial_buy_amount == 0
        && ctx.accounts.curve_state.real_sol_reserves == 0
        && ctx.accounts.buyer.key() == ctx.accounts.launch_state.creator
    {
        ctx.accounts.launch_state.initial_buy_amount = tokens_out;
    }

    // ── Transfers ───────────────────────────────────────────────────────────
    // SOL: buyer → sol_vault (sol_in) + buyer → fee_vault (platform_fee)
    let transfer_ix = anchor_lang::solana_program::system_instruction::transfer(
        &ctx.accounts.buyer.key(),
        &ctx.accounts.sol_vault.key(),
        sol_in,
    );
    anchor_lang::solana_program::program::invoke(
        &transfer_ix,
        &[
            ctx.accounts.buyer.to_account_info(),
            ctx.accounts.sol_vault.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
        ],
    )?;

    if platform_fee > 0 {
        let fee_ix = anchor_lang::solana_program::system_instruction::transfer(
            &ctx.accounts.buyer.key(),
            &ctx.accounts.fee_vault.key(),
            platform_fee,
        );
        anchor_lang::solana_program::program::invoke(
            &fee_ix,
            &[
                ctx.accounts.buyer.to_account_info(),
                ctx.accounts.fee_vault.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
        )?;
    }

    // Tokens: curve_token_vault → buyer (1:1, no fee withhold).
    let mint_key = ctx.accounts.mint.key();
    let vault_seeds: &[&[u8]] = &[
        b"curve_vault",
        mint_key.as_ref(),
        &[ctx.accounts.curve_state.vault_bump],
    ];
    anchor_spl::token_2022::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            anchor_spl::token_2022::TransferChecked {
                from: ctx.accounts.curve_token_vault.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.buyer_token_account.to_account_info(),
                authority: ctx.accounts.curve_token_vault.to_account_info(),
            },
            &[vault_seeds],
        ),
        tokens_out,
        ctx.accounts.mint.decimals,
    )?;

    // ── State updates ───────────────────────────────────────────────────────
    // Seal the blocklist on the first buy. After this point, the creator
    // cannot add or remove blocklisted addresses for the rest of the curve's
    // lifetime — defense against the post-launch rug where a creator would
    // wait for buyers, then add their wallets to the blocklist to trap their
    // funds. `is_first_buy` is captured BEFORE the curve mutation; checked
    // against pre-mutation `real_token_reserves` (== 0 iff no buys yet).
    let is_first_buy = ctx.accounts.curve_state.real_token_reserves == 0;
    if is_first_buy && !ctx.accounts.blocklist.frozen {
        ctx.accounts.blocklist.frozen = true;
    }

    let curve = &mut ctx.accounts.curve_state;
    curve.real_sol_reserves = curve
        .real_sol_reserves
        .checked_add(sol_in)
        .ok_or(LaunchCtrlError::MathOverflow)?;
    curve.real_token_reserves = curve
        .real_token_reserves
        .checked_add(tokens_out)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    if curve.real_sol_reserves >= curve.migration_threshold_lamports {
        curve.is_complete = true;
        emit!(CurveComplete {
            mint: ctx.accounts.mint.key(),
            final_sol: curve.real_sol_reserves,
        });
    }

    if curve.last_buy_slot == current_slot {
        curve.buys_this_slot = curve.buys_this_slot.saturating_add(1);
    } else {
        curve.last_buy_slot = current_slot;
        curve.buys_this_slot = 1;
    }

    emit!(TokensBought {
        mint: ctx.accounts.mint.key(),
        buyer: ctx.accounts.buyer.key(),
        sol_in,
        tokens_out,
        platform_fee,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct Buy<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,

    #[account(
        constraint = mint.to_account_info().owner == &spl_token_2022::ID
            @ LaunchCtrlError::InvalidMintOwner
    )]
    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        seeds = [b"curve", mint.key().as_ref()],
        bump = curve_state.bump,
        has_one = mint,
    )]
    pub curve_state: Account<'info, CurveState>,

    /// Mut so we can flip `frozen = true` on the first buy. After the first
    /// buy, blocklist.rs rejects any add/remove calls — the list is sealed
    /// for the rest of the curve's lifetime.
    #[account(
        mut,
        seeds = [b"blocklist", mint.key().as_ref()],
        bump = blocklist.bump,
    )]
    pub blocklist: Account<'info, Blocklist>,

    /// LaunchState — read on every buy (creator key for the initial-buy
    /// capture). `mut` because the very first buy of the curve writes
    /// `initial_buy_amount` for the deployer-bonus eligibility track.
    /// `Box`-wrapped per CLAUDE.md: large account structs need Box to avoid
    /// stack overflow.
    #[account(
        mut,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_state.bump,
        has_one = mint,
    )]
    pub launch_state: Box<Account<'info, LaunchState>>,

    #[account(
        mut,
        seeds = [b"curve_vault", mint.key().as_ref()],
        bump = curve_state.vault_bump,
    )]
    pub curve_token_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"sol_vault", mint.key().as_ref()],
        bump = curve_state.sol_vault_bump,
    )]
    /// CHECK: PDA — validated by seeds.
    pub sol_vault: UncheckedAccount<'info>,

    /// CHECK: Must match protocol-controlled fee vault — prevents buy-fee redirection.
    #[account(
        mut,
        constraint = fee_vault.key() == PLATFORM_FEE_VAULT @ LaunchCtrlError::Unauthorized,
    )]
    pub fee_vault: UncheckedAccount<'info>,

    #[account(
        mut,
        token::mint = mint,
        token::authority = buyer,
    )]
    pub buyer_token_account: InterfaceAccount<'info, TokenAccount>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token2022>,

    /// Optional Ricochet enforcement config. Pass `launchctrl::ID` as the
    /// account placeholder when Ricochet is disabled for this launch.
    pub ricochet_config: Option<Account<'info, RicochetConfig>>,

    /// CHECK: Validated by address constraint.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
}
