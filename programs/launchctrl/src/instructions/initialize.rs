use anchor_lang::prelude::*;
use anchor_spl::token_interface::spl_token_2022;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::Mint;

use crate::constants::*;
use crate::errors::LaunchCtrlError;
use crate::events::*;
use crate::state::*;

/// Record metadata for a Token-2022 mint that was deployed by the client
/// with **no extensions**. The mint is created client-side because Anchor's
/// `init` doesn't yet support Token-2022 extension parsing, but we don't
/// need any extensions in the FEE_REWORK design — fees are SOL-only and the
/// mint can be vanilla Token-2022.
///
/// Pre-rework launches had `[TransferFeeConfig]`. We reject any mint that
/// still has extensions to ensure new launches conform to the rework.
pub fn initialize_launch(
    ctx: Context<InitializeLaunch>,
    params: LaunchParams,
) -> Result<()> {
    require!(params.name.len() <= 32, LaunchCtrlError::NameTooLong);
    require!(params.symbol.len() <= 10, LaunchCtrlError::SymbolTooLong);
    require!(params.uri.len() <= 200, LaunchCtrlError::UriTooLong);
    require!(params.total_supply > 0, LaunchCtrlError::InvalidSupply);

    // Bound creator-supplied `migration_threshold_lamports` to a sane range.
    // Zero falls through to DEFAULT_MIGRATION_THRESHOLD_LAMPORTS (set on the
    // CurveState in `initialize_curve`); any non-zero value must sit between
    // MIN and MAX. Without this, the creator could trap or front-run buyers
    // by setting the threshold absurdly high or low. Source-of-truth for the
    // migration trigger is CurveState, but we mirror the same bounds on
    // LaunchState for defense-in-depth.
    if params.migration_threshold_lamports != 0 {
        require!(
            params.migration_threshold_lamports >= MIN_MIGRATION_THRESHOLD_LAMPORTS
                && params.migration_threshold_lamports <= MAX_MIGRATION_THRESHOLD_LAMPORTS,
            LaunchCtrlError::MigrationThresholdOutOfRange
        );
    }

    // Validate caller-supplied decay schedule:
    //   - length ≤ MAX_DECAY_SCHEDULE_LEN — LaunchState's allocated space
    //     fits 10 entries; an explicit cap rejects oversized inputs cleanly.
    //   - fee_bps ≤ MAX_DECAY_FEE_BPS (5000 = 50%) — sell.rs floors at
    //     BASE_FEE_BPS (1%) and applies the schedule on top. The previous
    //     10_000 bps cap let a creator forfeit 100% of every sell.
    //   - seconds_after_launch strictly increasing — current_decay_bps in
    //     sell.rs walks the schedule in order and `break`s at the first
    //     future step; out-of-order entries silently skip windows.
    require!(
        params.decay_schedule.len() <= MAX_DECAY_SCHEDULE_LEN,
        LaunchCtrlError::InvalidDecaySchedule
    );
    let mut prev_secs: u64 = 0;
    for (i, step) in params.decay_schedule.iter().enumerate() {
        require!(step.fee_bps <= MAX_DECAY_FEE_BPS, LaunchCtrlError::InvalidDecayBps);
        if i > 0 {
            require!(
                step.seconds_after_launch > prev_secs,
                LaunchCtrlError::InvalidDecaySchedule
            );
        }
        prev_secs = step.seconds_after_launch;
    }

    // Mint validation (FEE_REWORK April 2026): reject any mint with extensions.
    // The pre-rework code required exactly [TransferFeeConfig @ 50bps]; the new
    // code requires zero extensions. Vanilla Token-2022 only.
    {
        use spl_token_2022::extension::{BaseStateWithExtensions, StateWithExtensions, ExtensionType};

        let mint_account_info = ctx.accounts.mint.to_account_info();
        let mint_data = mint_account_info.data.borrow();
        let mint_state = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(&mint_data)
            .map_err(|_| error!(LaunchCtrlError::InvalidMintOwner))?;

        let extensions: Vec<ExtensionType> = mint_state
            .get_extension_types()
            .map_err(|_| error!(LaunchCtrlError::MintHasUnexpectedExtensions))?;

        require!(
            extensions.is_empty(),
            LaunchCtrlError::MintHasUnexpectedExtensions
        );
    }

    let clock = Clock::get()?;

    let schedule: Vec<DecayStep> = if params.decay_schedule.is_empty() {
        DEFAULT_DECAY_SCHEDULE
            .iter()
            .map(|&(secs, bps)| DecayStep {
                seconds_after_launch: secs,
                fee_bps: bps,
            })
            .collect()
    } else {
        params.decay_schedule.clone()
    };

    let launch = &mut ctx.accounts.launch_state;
    launch.creator = ctx.accounts.creator.key();
    launch.mint = ctx.accounts.mint.key();
    launch.name = params.name.clone();
    launch.symbol = params.symbol.clone();
    launch.uri = params.uri.clone();
    launch.total_supply = params.total_supply;
    launch.decimals = params.decimals;
    launch.launch_timestamp = clock.unix_timestamp;
    launch.migration_threshold_lamports = params.migration_threshold_lamports;
    launch.is_migrated = false;
    launch.decay_schedule = schedule;
    launch.bump = ctx.bumps.launch_state;
    // FEE_REWORK: no fee_authority PDA in the new flow; vestigial field set to 0
    // to preserve LaunchState layout for deserialization compatibility.
    launch.fee_authority_bump = 0;
    launch.migration_timestamp = 0;
    launch.drip_total = 0;
    launch.drip_injected = 0;
    launch.meteora_pool = Pubkey::default();
    launch.drip_vault_bump = 0;
    launch.migration_sol_vault_bump = 0;

    emit!(LaunchCreated {
        mint: ctx.accounts.mint.key(),
        creator: ctx.accounts.creator.key(),
        name: params.name,
        symbol: params.symbol,
        launch_timestamp: clock.unix_timestamp,
    });

    Ok(())
}

/// Initialize the bonding curve state for a newly launched token.
/// Called immediately after `initialize_launch`.
///
/// Initializes (in addition to curve_state, sol_vault, blocklist):
///   - `curve_token_vault` — Token-2022 ATA owned by the vault PDA itself
///   - `lp_seed_sol_vault` — system-owned PDA, accumulates SOL fees
///   - `lp_seed_token_vault` — Token-2022 ATA, accumulates buyback tokens
pub fn initialize_curve(
    ctx: Context<InitializeCurve>,
    migration_threshold_lamports: u64,
) -> Result<()> {
    let mint_key = ctx.accounts.mint.key();

    // ── 1. Curve token vault (Token-2022 account, vanilla — no extensions) ──
    // Manually created to keep try_accounts stack frame under the BPF 4 KiB
    // limit. New mints have no extensions, so 165-byte base account suffices.
    let curve_vault_bump = ctx.bumps.curve_token_vault;
    let curve_vault_seeds: &[&[u8]] =
        &[b"curve_vault", mint_key.as_ref(), &[curve_vault_bump]];

    anchor_lang::solana_program::program::invoke_signed(
        &anchor_lang::solana_program::system_instruction::create_account(
            ctx.accounts.creator.key,
            ctx.accounts.curve_token_vault.key,
            Rent::get()?.minimum_balance(165),
            165,
            &spl_token_2022::ID,
        ),
        &[
            ctx.accounts.creator.to_account_info(),
            ctx.accounts.curve_token_vault.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
        ],
        &[curve_vault_seeds],
    )?;
    anchor_lang::solana_program::program::invoke(
        &spl_token_2022::instruction::initialize_account3(
            &spl_token_2022::ID,
            ctx.accounts.curve_token_vault.key,
            ctx.accounts.mint.key,
            ctx.accounts.curve_token_vault.key, // self-authority via PDA seeds
        )?,
        &[
            ctx.accounts.curve_token_vault.to_account_info(),
            ctx.accounts.mint.to_account_info(),
        ],
    )?;

    // ── 2. LP-seed token vault (FEE_REWORK April 2026) ──────────────────────
    // Owner = migration_authority PDA (NOT self-authority). Meteora's
    // add_liquidity (called from claim_and_reinject post-migration) signs as
    // migration_authority, so the source ATA's Token-2022 owner field must
    // match. Self-authority worked for sell.rs (passive receive) and worked
    // for migrate.rs (we could sign with self-seeds), but Meteora's CPI
    // signs with the position owner only — hence the change.
    let lp_seed_token_bump = ctx.bumps.lp_seed_token_vault;
    let lp_seed_token_seeds: &[&[u8]] =
        &[LP_SEED_TOKEN_VAULT_SEED, mint_key.as_ref(), &[lp_seed_token_bump]];

    let (migration_authority_key, _) =
        Pubkey::find_program_address(&[b"migration_authority", mint_key.as_ref()], &crate::ID);

    anchor_lang::solana_program::program::invoke_signed(
        &anchor_lang::solana_program::system_instruction::create_account(
            ctx.accounts.creator.key,
            ctx.accounts.lp_seed_token_vault.key,
            Rent::get()?.minimum_balance(165),
            165,
            &spl_token_2022::ID,
        ),
        &[
            ctx.accounts.creator.to_account_info(),
            ctx.accounts.lp_seed_token_vault.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
        ],
        &[lp_seed_token_seeds],
    )?;
    anchor_lang::solana_program::program::invoke(
        &spl_token_2022::instruction::initialize_account3(
            &spl_token_2022::ID,
            ctx.accounts.lp_seed_token_vault.key,
            ctx.accounts.mint.key,
            &migration_authority_key, // owner = migration_authority for Meteora add_liquidity CPIs
        )?,
        &[
            ctx.accounts.lp_seed_token_vault.to_account_info(),
            ctx.accounts.mint.to_account_info(),
        ],
    )?;

    // ── 3. Curve state ─────────────────────────────────────────────────────
    let curve = &mut ctx.accounts.curve_state;
    curve.mint = *ctx.accounts.mint.key;
    curve.creator = ctx.accounts.creator.key();
    curve.virtual_sol_reserves = VIRTUAL_SOL_RESERVES;
    curve.virtual_token_reserves = VIRTUAL_TOKEN_RESERVES;
    curve.real_sol_reserves = 0;
    curve.real_token_reserves = 0;
    // Same bounds as initialize_launch. CurveState is the source-of-truth
    // for the migration trigger (buy.rs reads this field), so the gate here
    // is the load-bearing one.
    curve.migration_threshold_lamports = if migration_threshold_lamports == 0 {
        DEFAULT_MIGRATION_THRESHOLD_LAMPORTS
    } else {
        require!(
            migration_threshold_lamports >= MIN_MIGRATION_THRESHOLD_LAMPORTS
                && migration_threshold_lamports <= MAX_MIGRATION_THRESHOLD_LAMPORTS,
            LaunchCtrlError::MigrationThresholdOutOfRange
        );
        migration_threshold_lamports
    };
    curve.is_complete = false;
    curve.is_funds_released = false;
    curve.bump = ctx.bumps.curve_state;
    curve.vault_bump = ctx.bumps.curve_token_vault;
    curve.sol_vault_bump = ctx.bumps.sol_vault;
    curve.creation_slot = Clock::get()?.slot;
    curve.last_buy_slot = 0;
    curve.buys_this_slot = 0;

    let blocklist = &mut ctx.accounts.blocklist;
    blocklist.mint = ctx.accounts.mint.key();
    blocklist.creator = ctx.accounts.creator.key();
    blocklist.blocked = Vec::new();
    blocklist.bump = ctx.bumps.blocklist;
    // Mutable until the first buy seals it (see buy.rs). Creator must finish
    // populating the blocklist before any buyer arrives.
    blocklist.frozen = false;

    emit!(CurveInitialized {
        mint: ctx.accounts.mint.key(),
        creator: ctx.accounts.creator.key(),
        migration_threshold_lamports: curve.migration_threshold_lamports,
    });

    Ok(())
}

// ─── Accounts ────────────────────────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(params: LaunchParams)]
pub struct InitializeLaunch<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,

    #[account(
        constraint = mint.to_account_info().owner == &spl_token_2022::ID
            @ LaunchCtrlError::InvalidMintOwner
    )]
    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        init,
        payer = creator,
        space = LaunchState::LEN,
        seeds = [b"launch", mint.key().as_ref()],
        bump,
    )]
    pub launch_state: Account<'info, LaunchState>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token2022>,
}

#[derive(Accounts)]
pub struct InitializeCurve<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,

    /// CHECK: Owner verified to be Token-2022 program via constraint.
    #[account(
        constraint = mint.owner == &spl_token_2022::ID @ LaunchCtrlError::InvalidMintOwner
    )]
    pub mint: UncheckedAccount<'info>,

    #[account(
        init,
        payer = creator,
        space = CurveState::LEN,
        seeds = [b"curve", mint.key().as_ref()],
        bump,
    )]
    pub curve_state: Box<Account<'info, CurveState>>,

    /// CHECK: Token vault PDA — created and initialized as a Token-2022 token
    /// account inside the instruction body to keep try_accounts under the BPF
    /// 4 KiB stack limit. Self-authority via the PDA's own seeds.
    #[account(
        mut,
        seeds = [b"curve_vault", mint.key().as_ref()],
        bump,
    )]
    pub curve_token_vault: UncheckedAccount<'info>,

    /// SOL vault — program-owned PDA holding bonding curve SOL reserves.
    #[account(
        init,
        payer = creator,
        space = 8,
        seeds = [b"sol_vault", mint.key().as_ref()],
        bump,
    )]
    pub sol_vault: Account<'info, SolVault>,

    /// LP-seed SOL accumulator (FEE_REWORK April 2026). Receives sell-side
    /// fees during the bonding curve and post-mig LP-fee splits.
    #[account(
        init,
        payer = creator,
        space = 8,
        seeds = [LP_SEED_SOL_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub lp_seed_sol_vault: Account<'info, LpSeedSolVault>,

    /// CHECK: LP-seed token vault — created + initialized inside the body
    /// (same pattern as curve_token_vault). Receives inline-buyback tokens
    /// during the bonding curve and project-token claims post-mig.
    #[account(
        mut,
        seeds = [LP_SEED_TOKEN_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub lp_seed_token_vault: UncheckedAccount<'info>,

    /// Per-token blocklist — initialized empty; creator can populate via
    /// add_to_blocklist. Pre-allocated for MAX_BLOCKLIST_SIZE.
    #[account(
        init,
        payer = creator,
        space = Blocklist::space(MAX_BLOCKLIST_SIZE),
        seeds = [b"blocklist", mint.key().as_ref()],
        bump,
    )]
    pub blocklist: Box<Account<'info, Blocklist>>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token2022>,
}
