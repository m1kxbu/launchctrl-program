use anchor_lang::prelude::*;
use anchor_spl::token_interface::spl_token_2022;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{Mint, TokenAccount};

use crate::constants::*;
use crate::errors::LaunchCtrlError;
use crate::events::*;
use crate::state::*;

/// Drain the bonding curve's SOL + token reserves AND the FEE_REWORK
/// `lp_seed_*` accumulators into program-owned migration PDAs, then mark
/// the launch as migrated.
///
/// Permissionless — any wallet can trigger once `curve_state.is_complete`.
pub fn migrate_to_pool(ctx: Context<MigrateToPool>) -> Result<()> {
    let mint_key = ctx.accounts.mint.key();
    let vault_bump = ctx.bumps.migration_token_vault;
    let sol_bump = ctx.bumps.migration_sol_vault;

    // ── 1. Create + initialize migration_token_vault (Token-2022, no extensions) ─
    let vault_seeds: &[&[u8]] = &[b"migration_vault", mint_key.as_ref(), &[vault_bump]];
    anchor_lang::solana_program::program::invoke_signed(
        &anchor_lang::solana_program::system_instruction::create_account(
            ctx.accounts.cranker.key,
            &ctx.accounts.migration_token_vault.key(),
            Rent::get()?.minimum_balance(165),
            165u64,
            &spl_token_2022::ID,
        ),
        &[
            ctx.accounts.cranker.to_account_info(),
            ctx.accounts.migration_token_vault.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
        ],
        &[vault_seeds],
    )?;
    anchor_lang::solana_program::program::invoke(
        &spl_token_2022::instruction::initialize_account3(
            &spl_token_2022::ID,
            &ctx.accounts.migration_token_vault.key(),
            &ctx.accounts.mint.key(),
            ctx.accounts.migration_authority.key,
        )?,
        &[
            ctx.accounts.migration_token_vault.to_account_info(),
            ctx.accounts.mint.to_account_info(),
        ],
    )?;

    // ── 2. Drain curve_token_vault → migration_token_vault ──────────────────
    require!(ctx.accounts.curve_state.is_complete, LaunchCtrlError::CurveNotComplete);
    require!(!ctx.accounts.curve_state.is_funds_released, LaunchCtrlError::AlreadyReleased);

    let curve_token_amount = ctx.accounts.curve_token_vault.amount;
    if curve_token_amount > 0 {
        let curve_vault_seeds: &[&[u8]] = &[
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
                    to: ctx.accounts.migration_token_vault.to_account_info(),
                    authority: ctx.accounts.curve_token_vault.to_account_info(),
                },
                &[curve_vault_seeds],
            ),
            curve_token_amount,
            ctx.accounts.mint.decimals,
        )?;
    }

    // ── 3. Drain lp_seed_token_vault → migration_token_vault (FEE_REWORK) ───
    // lp_seed_token_vault.owner = migration_authority (set in initialize_curve
    // so Meteora's add_liquidity can pull from it post-migration). Sign with
    // migration_authority's PDA seeds.
    let lp_seed_token_amount = ctx.accounts.lp_seed_token_vault.amount;
    if lp_seed_token_amount > 0 {
        let mig_auth_bump = ctx.bumps.migration_authority;
        let mig_auth_seeds: &[&[u8]] =
            &[b"migration_authority", mint_key.as_ref(), &[mig_auth_bump]];
        anchor_spl::token_2022::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                anchor_spl::token_2022::TransferChecked {
                    from: ctx.accounts.lp_seed_token_vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.migration_token_vault.to_account_info(),
                    authority: ctx.accounts.migration_authority.to_account_info(),
                },
                &[mig_auth_seeds],
            ),
            lp_seed_token_amount,
            ctx.accounts.mint.decimals,
        )?;
    }

    // ── 4. Drain sol_vault + lp_seed_sol_vault → migration_sol_vault ────────
    // All CPIs done; raw lamport mutation on program-owned PDAs is safe.
    let curve_sol = ctx.accounts.sol_vault.to_account_info().lamports();
    **ctx.accounts.sol_vault.to_account_info().try_borrow_mut_lamports()? = 0;
    **ctx.accounts.migration_sol_vault.to_account_info().try_borrow_mut_lamports()? = ctx
        .accounts
        .migration_sol_vault
        .to_account_info()
        .lamports()
        .checked_add(curve_sol)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    let lp_seed_sol = ctx.accounts.lp_seed_sol_vault.to_account_info().lamports();
    if lp_seed_sol > 0 {
        **ctx.accounts.lp_seed_sol_vault.to_account_info().try_borrow_mut_lamports()? = 0;
        **ctx.accounts.migration_sol_vault.to_account_info().try_borrow_mut_lamports()? = ctx
            .accounts
            .migration_sol_vault
            .to_account_info()
            .lamports()
            .checked_add(lp_seed_sol)
            .ok_or(LaunchCtrlError::MathOverflow)?;
    }

    ctx.accounts.curve_state.is_funds_released = true;

    emit!(FundsReleased {
        mint: mint_key,
        sol_amount: curve_sol.checked_add(lp_seed_sol).ok_or(LaunchCtrlError::MathOverflow)?,
        token_amount: curve_token_amount.checked_add(lp_seed_token_amount).ok_or(LaunchCtrlError::MathOverflow)?,
    });

    // ── 5. Mark launch migrated ─────────────────────────────────────────────
    let clock = Clock::get()?;
    let launch = &mut ctx.accounts.launch_state;
    launch.is_migrated = true;
    launch.migration_timestamp = clock.unix_timestamp;
    launch.drip_vault_bump = vault_bump;
    launch.migration_sol_vault_bump = sol_bump;

    emit!(MigrationComplete {
        mint: mint_key,
        migration_timestamp: clock.unix_timestamp,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct MigrateToPool<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,

    #[account(
        constraint = mint.to_account_info().owner == &spl_token_2022::ID
            @ LaunchCtrlError::InvalidMintOwner
    )]
    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_state.bump,
        constraint = !launch_state.is_migrated @ LaunchCtrlError::AlreadyMigrated,
    )]
    pub launch_state: Account<'info, LaunchState>,

    #[account(
        mut,
        seeds = [b"curve", mint.key().as_ref()],
        bump = curve_state.bump,
        has_one = mint,
    )]
    pub curve_state: Account<'info, CurveState>,

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
    pub sol_vault: Account<'info, SolVault>,

    /// LP-seed SOL accumulator — drained into migration_sol_vault.
    #[account(
        mut,
        seeds = [LP_SEED_SOL_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub lp_seed_sol_vault: Account<'info, LpSeedSolVault>,

    /// LP-seed token accumulator — drained into migration_token_vault.
    #[account(
        mut,
        seeds = [LP_SEED_TOKEN_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub lp_seed_token_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        init,
        payer = cranker,
        space = 8,
        seeds = [b"migration_sol", mint.key().as_ref()],
        bump,
    )]
    pub migration_sol_vault: Account<'info, MigrationSolVault>,

    /// CHECK: Created and initialized via invoke_signed inside migrate_to_pool.
    #[account(
        mut,
        seeds = [b"migration_vault", mint.key().as_ref()],
        bump,
    )]
    pub migration_token_vault: UncheckedAccount<'info>,

    /// CHECK: PDA — becomes authority over migration_token_vault.
    #[account(
        seeds = [b"migration_authority", mint.key().as_ref()],
        bump,
    )]
    pub migration_authority: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token2022>,
    pub system_program: Program<'info, System>,
}
