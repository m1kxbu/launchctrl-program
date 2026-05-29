use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_spl::token_interface::spl_token_2022;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{Mint, TokenAccount};

use crate::constants::*;
use crate::errors::LaunchCtrlError;
use crate::events::{LpReinjected, RewardsDiverted, DevBonusDiverted};
use crate::state::*;

/// LP flywheel crank — permissionless. Anyone can call this after migration +
/// pool creation to recycle Meteora LP fees back into the locked position
/// AND fund the protocol vaults (FEE_REWORK April 2026 / NEST_EGG_TO_DEV_BONUS Phase 3):
///
///   1. CPI `Meteora::claim_position_fee` — pool uses `collect_fee_mode = 0`
///      (BothToken). Project-token fees land in `lp_seed_token_vault` (which
///      replaces the old `drip_vault`); SOL fees land in the ephemeral
///      `wsol_vault`.
///   2. CPI `Meteora::add_liquidity` — consumes `liquidity_delta` Q64.64
///      units, sized off-chain so SOL consumption ≈ LP_REINJECT_BPS (62.5%)
///      of `wsol_claimed`. The remaining 37.5% is the residual.
///   3. Close `wsol_vault` → residual + rent dumped into `migration_sol_vault`.
///   4. Compute eligibility: dev is "still holding" iff
///        `launch.initial_buy_amount > 0 && creator_token_account.amount >= launch.initial_buy_amount`.
///   5. Raw-debit the 4-way split out of `migration_sol_vault`:
///        25%   → reward_vault   (Diamond Hands holder pool)
///        6.25% → deployer_vault (when eligible) OR rolled into reward_vault (when not)
///        6.25% → fee_vault      (platform revenue)
///        62.5% already consumed by add_liquidity above.
///   6. Refund cranker rent.
///
/// All raw lamport mutations happen AFTER all CPIs (lamport-conservation rule).
/// Lamport-conservation invariant: lp_take + reward_take + dev_bonus_take +
/// platform_take == wsol_claimed (modulo floor-rounding on integer division),
/// in BOTH eligibility branches — the branch only changes WHICH vault gets
/// the dev_bonus_take, not the sum.
pub fn claim_and_reinject(
    ctx: Context<ClaimAndReinject>,
    liquidity_delta: u128,
) -> Result<()> {
    let mint_key = ctx.accounts.mint.key();
    let mig_auth_bump = ctx.bumps.migration_authority;
    let wsol_vault_bump = ctx.bumps.wsol_vault;

    let migration_auth_seeds: &[&[u8]] =
        &[b"migration_authority", mint_key.as_ref(), &[mig_auth_bump]];
    let wsol_vault_seeds: &[&[u8]] =
        &[b"wsol_vault", mint_key.as_ref(), &[wsol_vault_bump]];

    // ── 1. Create ephemeral WSOL vault (cranker pays rent) ──────────────────
    let wsol_rent = Rent::get()?.minimum_balance(165);
    anchor_lang::solana_program::program::invoke_signed(
        &anchor_lang::solana_program::system_instruction::create_account(
            ctx.accounts.cranker.key,
            ctx.accounts.wsol_vault.key,
            wsol_rent,
            165,
            &SPL_TOKEN_PROGRAM_ID,
        ),
        &[
            ctx.accounts.cranker.to_account_info(),
            ctx.accounts.wsol_vault.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
        ],
        &[wsol_vault_seeds],
    )?;
    {
        let mut init_data = vec![18u8]; // initialize_account3
        init_data.extend_from_slice(ctx.accounts.migration_authority.key.as_ref());
        anchor_lang::solana_program::program::invoke(
            &Instruction {
                program_id: SPL_TOKEN_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new(*ctx.accounts.wsol_vault.key, false),
                    AccountMeta::new_readonly(WSOL_MINT, false),
                ],
                data: init_data,
            },
            &[
                ctx.accounts.wsol_vault.to_account_info(),
                ctx.accounts.wsol_mint.to_account_info(),
            ],
        )?;
    }

    // ── 2. Sort mints (Meteora canonical order) ─────────────────────────────
    let wsol_is_token_a = WSOL_MINT.to_bytes() < mint_key.to_bytes();
    let (token_a_mint_key, token_b_mint_key) = if wsol_is_token_a {
        (WSOL_MINT, mint_key)
    } else {
        (mint_key, WSOL_MINT)
    };

    let wsol_vault_info        = ctx.accounts.wsol_vault.to_account_info();
    let lp_seed_token_info     = ctx.accounts.lp_seed_token_vault.to_account_info();
    let wsol_mint_info         = ctx.accounts.wsol_mint.to_account_info();
    let project_mint_info      = ctx.accounts.mint.to_account_info();
    let spl_token_prog_info    = ctx.accounts.spl_token_program.to_account_info();
    let t22_token_prog_info    = ctx.accounts.token_2022_program.to_account_info();

    let (token_a_account, token_b_account) = if wsol_is_token_a {
        (wsol_vault_info.clone(), lp_seed_token_info.clone())
    } else {
        (lp_seed_token_info.clone(), wsol_vault_info.clone())
    };
    let (token_a_mint_info, token_b_mint_info) = if wsol_is_token_a {
        (wsol_mint_info.clone(), project_mint_info.clone())
    } else {
        (project_mint_info.clone(), wsol_mint_info.clone())
    };
    let (token_a_prog, token_b_prog) = if wsol_is_token_a {
        (spl_token_prog_info.clone(), t22_token_prog_info.clone())
    } else {
        (t22_token_prog_info.clone(), spl_token_prog_info.clone())
    };

    // ── 3. CPI: Meteora claim_position_fee ──────────────────────────────────
    anchor_lang::solana_program::program::invoke_signed(
        &Instruction {
            program_id: METEORA_DAMM_V2_PROGRAM,
            accounts: vec![
                AccountMeta::new_readonly(ctx.accounts.pool_authority.key(),       false),
                AccountMeta::new_readonly(ctx.accounts.meteora_pool.key(),         false),
                AccountMeta::new(ctx.accounts.meteora_position.key(),              false),
                AccountMeta::new(token_a_account.key(),                            false),
                AccountMeta::new(token_b_account.key(),                            false),
                AccountMeta::new(ctx.accounts.token_a_vault.key(),                 false),
                AccountMeta::new(ctx.accounts.token_b_vault.key(),                 false),
                AccountMeta::new_readonly(token_a_mint_key,                        false),
                AccountMeta::new_readonly(token_b_mint_key,                        false),
                AccountMeta::new_readonly(ctx.accounts.position_nft_account.key(), false),
                AccountMeta::new_readonly(ctx.accounts.migration_authority.key(),  true),
                AccountMeta::new_readonly(token_a_prog.key(),                      false),
                AccountMeta::new_readonly(token_b_prog.key(),                      false),
                AccountMeta::new_readonly(ctx.accounts.event_authority.key(),      false),
                AccountMeta::new_readonly(ctx.accounts.meteora_program.key(),      false),
            ],
            data: DAMM_V2_DISC_CLAIM_POSITION_FEE.to_vec(),
        },
        &[
            ctx.accounts.pool_authority.to_account_info(),
            ctx.accounts.meteora_pool.to_account_info(),
            ctx.accounts.meteora_position.to_account_info(),
            token_a_account.clone(),
            token_b_account.clone(),
            ctx.accounts.token_a_vault.to_account_info(),
            ctx.accounts.token_b_vault.to_account_info(),
            token_a_mint_info.clone(),
            token_b_mint_info.clone(),
            ctx.accounts.position_nft_account.to_account_info(),
            ctx.accounts.migration_authority.to_account_info(),
            token_a_prog.clone(),
            token_b_prog.clone(),
            ctx.accounts.event_authority.to_account_info(),
            ctx.accounts.meteora_program.to_account_info(),
        ],
        &[migration_auth_seeds],
    )?;

    // Measure post-claim balances.
    let wsol_claimed: u64 = {
        let data = ctx.accounts.wsol_vault.try_borrow_data()?;
        require!(data.len() >= 72, LaunchCtrlError::InvalidProgram);
        u64::from_le_bytes(data[64..72].try_into().unwrap())
    };
    ctx.accounts.lp_seed_token_vault.reload()?;
    let lp_seed_tokens_before = ctx.accounts.lp_seed_token_vault.amount;

    // 4-way split of wsol_claimed (computed up front; mutations at step 7).
    let split = |bps: u64| -> Result<u64> {
        Ok((wsol_claimed as u128)
            .checked_mul(bps as u128)
            .and_then(|v| v.checked_div(10_000u128))
            .ok_or(LaunchCtrlError::MathOverflow)? as u64)
    };
    let lp_take         = split(LP_REINJECT_BPS)?;  // 62.5%
    let reward_take     = split(REWARD_POOL_BPS)?;  // 25%
    let dev_bonus_take  = split(DEV_BONUS_BPS)?;    // 6.25%
    let platform_take   = split(PLATFORM_LP_BPS)?;  // 6.25%

    // ── Deployer-bonus eligibility ──────────────────────────────────────────
    // Cheap on-chain gate: creator must still be holding at least their
    // captured initial-buy size. Off-chain Diamond-Hands snapshot pipeline
    // (10-day rolling streak) gates the eventual *claim*, but the per-cycle
    // divert decision uses this lighter check to keep the crank cost bounded
    // and avoid coupling on-chain logic to off-chain state. If the creator
    // dumps mid-cycle and re-buys before the next sweep, that cycle is still
    // forfeited (eligibility evaluates fresh each sweep).
    //
    // initial_buy_amount > 0 closes the gameability hole where a creator
    // skipped the launch-tx initial buy (initial_buy_amount stays 0 forever)
    // — that path NEVER qualifies for the dev bonus.
    let creator_balance = ctx.accounts.creator_token_account.amount;
    let dev_bonus_eligible = ctx.accounts.launch_state.initial_buy_amount > 0
        && creator_balance >= ctx.accounts.launch_state.initial_buy_amount;

    // ── 4. CPI: Meteora add_liquidity ───────────────────────────────────────
    let mut add_liq_data = DAMM_V2_DISC_ADD_LIQUIDITY.to_vec();
    add_liq_data.extend_from_slice(&liquidity_delta.to_le_bytes());
    add_liq_data.extend_from_slice(&u64::MAX.to_le_bytes());
    add_liq_data.extend_from_slice(&u64::MAX.to_le_bytes());

    anchor_lang::solana_program::program::invoke_signed(
        &Instruction {
            program_id: METEORA_DAMM_V2_PROGRAM,
            accounts: vec![
                AccountMeta::new(ctx.accounts.meteora_pool.key(),                  false),
                AccountMeta::new(ctx.accounts.meteora_position.key(),              false),
                AccountMeta::new(token_a_account.key(),                            false),
                AccountMeta::new(token_b_account.key(),                            false),
                AccountMeta::new(ctx.accounts.token_a_vault.key(),                 false),
                AccountMeta::new(ctx.accounts.token_b_vault.key(),                 false),
                AccountMeta::new_readonly(token_a_mint_key,                        false),
                AccountMeta::new_readonly(token_b_mint_key,                        false),
                AccountMeta::new_readonly(ctx.accounts.position_nft_account.key(), false),
                AccountMeta::new_readonly(ctx.accounts.migration_authority.key(),  true),
                AccountMeta::new_readonly(token_a_prog.key(),                      false),
                AccountMeta::new_readonly(token_b_prog.key(),                      false),
                AccountMeta::new_readonly(ctx.accounts.event_authority.key(),      false),
                AccountMeta::new_readonly(ctx.accounts.meteora_program.key(),      false),
            ],
            data: add_liq_data,
        },
        &[
            ctx.accounts.meteora_pool.to_account_info(),
            ctx.accounts.meteora_position.to_account_info(),
            token_a_account.clone(),
            token_b_account.clone(),
            ctx.accounts.token_a_vault.to_account_info(),
            ctx.accounts.token_b_vault.to_account_info(),
            token_a_mint_info,
            token_b_mint_info,
            ctx.accounts.position_nft_account.to_account_info(),
            ctx.accounts.migration_authority.to_account_info(),
            token_a_prog,
            token_b_prog,
            ctx.accounts.event_authority.to_account_info(),
            ctx.accounts.meteora_program.to_account_info(),
        ],
        &[migration_auth_seeds],
    )?;

    // ── 4b. Sanity-check that add_liquidity actually consumed wSOL ──────────
    // claim_and_reinject is permissionless. A griefer cranker passing
    // `liquidity_delta = 0` (or negligibly small) would skip reinjection
    // entirely — the LP doesn't grow but the 4-way split below still pays
    // out platform/reward/nest_egg from migration_sol_vault. Block the
    // trivial form by requiring add_liquidity to consume at least 1 lamport
    // of wSOL when the claim returned non-zero. Permissive enough to allow
    // the bot to tune `DEPOSIT_FRACTION_BPS` without coupling, strict enough
    // to catch consumed-zero griefing.
    let wsol_after_add: u64 = {
        let data = ctx.accounts.wsol_vault.try_borrow_data()?;
        require!(data.len() >= 72, LaunchCtrlError::InvalidProgram);
        u64::from_le_bytes(data[64..72].try_into().unwrap())
    };
    let wsol_consumed = wsol_claimed.saturating_sub(wsol_after_add);
    if wsol_claimed > 0 {
        require!(wsol_consumed > 0, LaunchCtrlError::InsufficientLpReinjection);
    }

    // ── 5. Close WSOL vault → residual + rent → migration_sol_vault ─────────
    anchor_lang::solana_program::program::invoke_signed(
        &Instruction {
            program_id: SPL_TOKEN_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(*ctx.accounts.wsol_vault.key, false),
                AccountMeta::new(ctx.accounts.migration_sol_vault.key(), false),
                AccountMeta::new_readonly(*ctx.accounts.migration_authority.key, true),
            ],
            data: vec![9u8],
        },
        &[
            ctx.accounts.wsol_vault.to_account_info(),
            ctx.accounts.migration_sol_vault.to_account_info(),
            ctx.accounts.migration_authority.to_account_info(),
        ],
        &[migration_auth_seeds],
    )?;

    // ── 6. 4-way split: raw-debit out of migration_sol_vault ────────────────
    // All CPIs are done. The bot is required to size `liquidity_delta` so that
    // add_liquidity consumed ~LP_REINJECT_BPS (62.5%) of wsol_claimed; the
    // residual ~37.5% just flowed into migration_sol_vault on close. We split
    // it across three buckets (reward / deployer-bonus / platform). If the
    // bot under-sized, residual exceeds 37.5% and the surplus accumulates in
    // migration_sol_vault as protocol reserve. If the bot over-sized, the
    // checked_sub below errors (rare — and we fail loudly rather than silently
    // dip into protocol reserves).
    //
    // The dev-bonus branch:
    //   eligible      → divert dev_bonus_take to deployer_vault
    //   ineligible    → roll dev_bonus_take into reward_take (boosts holder pool)
    // Sum invariant `reward_take + dev_bonus_take + platform_take` is identical
    // in both branches; only the destination of the dev_bonus_take changes.
    let mig_sol_info = ctx.accounts.migration_sol_vault.to_account_info();
    let effective_reward_take = if dev_bonus_eligible {
        reward_take
    } else {
        reward_take
            .checked_add(dev_bonus_take)
            .ok_or(LaunchCtrlError::MathOverflow)?
    };

    if effective_reward_take > 0 {
        let dest = ctx.accounts.reward_vault.to_account_info();
        **mig_sol_info.try_borrow_mut_lamports()? = mig_sol_info
            .lamports()
            .checked_sub(effective_reward_take)
            .ok_or(LaunchCtrlError::InsufficientRewardVaultBalance)?;
        **dest.try_borrow_mut_lamports()? = dest
            .lamports()
            .checked_add(effective_reward_take)
            .ok_or(LaunchCtrlError::MathOverflow)?;
        emit!(RewardsDiverted {
            mint: mint_key,
            amount_lamports: effective_reward_take,
            reward_vault: ctx.accounts.reward_vault.key(),
        });
    }

    if dev_bonus_eligible && dev_bonus_take > 0 {
        let dest = ctx.accounts.deployer_vault.to_account_info();
        **mig_sol_info.try_borrow_mut_lamports()? = mig_sol_info
            .lamports()
            .checked_sub(dev_bonus_take)
            .ok_or(LaunchCtrlError::InsufficientDeployerVaultBalance)?;
        **dest.try_borrow_mut_lamports()? = dest
            .lamports()
            .checked_add(dev_bonus_take)
            .ok_or(LaunchCtrlError::MathOverflow)?;
    }
    // Always emit DevBonusDiverted (even when ineligible / amount=0) so
    // off-chain indexers can trace eligibility transitions per cycle.
    if dev_bonus_take > 0 {
        emit!(DevBonusDiverted {
            mint: mint_key,
            amount_lamports: dev_bonus_take,
            eligible: dev_bonus_eligible,
            deployer_vault: ctx.accounts.deployer_vault.key(),
        });
    }

    if platform_take > 0 {
        let dest = ctx.accounts.fee_vault.to_account_info();
        **mig_sol_info.try_borrow_mut_lamports()? = mig_sol_info
            .lamports()
            .checked_sub(platform_take)
            .ok_or(LaunchCtrlError::MathOverflow)?;
        **dest.try_borrow_mut_lamports()? = dest
            .lamports()
            .checked_add(platform_take)
            .ok_or(LaunchCtrlError::MathOverflow)?;
    }

    // ── 7. Refund wsol_vault rent to cranker ────────────────────────────────
    **mig_sol_info.try_borrow_mut_lamports()? = mig_sol_info
        .lamports()
        .checked_sub(wsol_rent)
        .ok_or(LaunchCtrlError::MathOverflow)?;
    **ctx.accounts.cranker.to_account_info().try_borrow_mut_lamports()? = ctx
        .accounts
        .cranker
        .lamports()
        .checked_add(wsol_rent)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    emit!(LpReinjected {
        mint: mint_key,
        liquidity_delta,
        lp_seed_tokens_before,
        wsol_claimed,
        lp_take,
        reward_take,       // computed share (pre-eligibility roll-up)
        dev_bonus_take,    // computed share (pre-eligibility roll-up)
        platform_take,
        dev_bonus_eligible,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct ClaimAndReinject<'info> {
    /// Permissionless cranker — fronts wsol_vault rent, refunded at the end.
    #[account(mut)]
    pub cranker: Signer<'info>,

    #[account(
        mut,
        constraint = mint.to_account_info().owner == &spl_token_2022::ID
            @ LaunchCtrlError::InvalidMintOwner
    )]
    pub mint: InterfaceAccount<'info, Mint>,

    /// CHECK: WSOL native mint.
    #[account(constraint = wsol_mint.key() == WSOL_MINT @ LaunchCtrlError::InvalidProgram)]
    pub wsol_mint: UncheckedAccount<'info>,

    /// Boxed because LaunchState is 525 bytes; stack-overflow safety as the
    /// struct grew with creator_token_account in NEST_EGG_TO_DEV_BONUS Phase 3.
    #[account(
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_state.bump,
        has_one = mint,
        has_one = creator,
        constraint = launch_state.is_migrated @ LaunchCtrlError::NotMigrated,
        constraint = launch_state.meteora_pool != Pubkey::default() @ LaunchCtrlError::PoolNotCreated,
    )]
    pub launch_state: Box<Account<'info, LaunchState>>,

    /// CHECK: Creator pubkey, used as the authority binding for `creator_token_account`.
    /// Cross-validated against `launch_state.creator` via the `has_one = creator`
    /// constraint above. Read-only; we don't sign with this account.
    pub creator: UncheckedAccount<'info>,

    /// Creator's associated token account for this mint. Used to read the
    /// current balance to evaluate dev-bonus eligibility. Constrained to:
    ///   - mint == this mint (cross-mint substitution blocked)
    ///   - authority == launch_state.creator (via the `creator` account above —
    ///     the `has_one = creator` constraint links them).
    /// Anchor verifies this BEFORE the handler runs, so a fake ATA fails
    /// constraint validation and never reaches the eligibility check.
    #[account(
        token::mint = mint,
        token::authority = creator,
    )]
    pub creator_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: PDA — signs Meteora CPIs as position owner.
    #[account(
        seeds = [b"migration_authority", mint.key().as_ref()],
        bump,
    )]
    pub migration_authority: UncheckedAccount<'info>,

    /// LP-seed token vault — replaces the legacy `drip_vault`. Receives
    /// project-token side of `claim_position_fee`; sourced by `add_liquidity`.
    #[account(
        mut,
        seeds = [LP_SEED_TOKEN_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub lp_seed_token_vault: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: Ephemeral WSOL vault — created, used, and closed inside this ix.
    #[account(
        mut,
        seeds = [b"wsol_vault", mint.key().as_ref()],
        bump,
    )]
    pub wsol_vault: UncheckedAccount<'info>,

    /// Program-owned SOL accumulator — receives wsol_vault residual + rent on
    /// close. Source for the 4-way split.
    #[account(
        mut,
        seeds = [b"migration_sol", mint.key().as_ref()],
        bump = launch_state.migration_sol_vault_bump,
    )]
    pub migration_sol_vault: Account<'info, MigrationSolVault>,

    /// Diamond Hands rewards accumulator. Receives 25% of wsol_claimed.
    #[account(
        mut,
        seeds = [REWARD_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub reward_vault: Account<'info, RewardVault>,

    /// Deployer Bonus accumulator (NEST_EGG_TO_DEV_BONUS Phase 3, replaces
    /// the legacy `nest_egg_vault`). Receives 6.25% of wsol_claimed when the
    /// creator still holds at least their `initial_buy_amount`. When
    /// ineligible, the slice rolls into `reward_vault` instead and this
    /// account's lamports are unchanged.
    #[account(
        mut,
        seeds = [DEPLOYER_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub deployer_vault: Account<'info, DeployerVault>,

    /// Platform revenue vault. Receives 6.25% of wsol_claimed.
    /// CHECK: Must match protocol-controlled fee vault.
    #[account(
        mut,
        constraint = fee_vault.key() == PLATFORM_FEE_VAULT @ LaunchCtrlError::Unauthorized,
    )]
    pub fee_vault: UncheckedAccount<'info>,

    // ─── Meteora DAMM v2 accounts ────────────────────────────────────────────

    /// CHECK: Must match the DAMM v2 pool recorded in launch_state.
    #[account(
        mut,
        constraint = meteora_pool.key() == launch_state.meteora_pool @ LaunchCtrlError::InvalidProgram,
    )]
    pub meteora_pool: UncheckedAccount<'info>,

    /// CHECK: DAMM v2 position PDA.
    #[account(mut)]
    pub meteora_position: UncheckedAccount<'info>,

    /// CHECK: Position NFT account.
    pub position_nft_account: UncheckedAccount<'info>,

    /// CHECK: Meteora pool token_a vault.
    #[account(mut)]
    pub token_a_vault: UncheckedAccount<'info>,

    /// CHECK: Meteora pool token_b vault.
    #[account(mut)]
    pub token_b_vault: UncheckedAccount<'info>,

    /// CHECK: Meteora DAMM v2 global pool_authority.
    #[account(constraint = pool_authority.key() == METEORA_POOL_AUTHORITY @ LaunchCtrlError::InvalidProgram)]
    pub pool_authority: UncheckedAccount<'info>,

    /// CHECK: Meteora DAMM v2 event_authority PDA.
    #[account(constraint = event_authority.key() == METEORA_EVENT_AUTHORITY @ LaunchCtrlError::InvalidProgram)]
    pub event_authority: UncheckedAccount<'info>,

    /// CHECK: Must match METEORA_DAMM_V2_PROGRAM.
    #[account(constraint = meteora_program.key() == METEORA_DAMM_V2_PROGRAM @ LaunchCtrlError::InvalidProgram)]
    pub meteora_program: UncheckedAccount<'info>,

    pub token_2022_program: Program<'info, Token2022>,

    /// CHECK: Classic SPL Token program — used for WSOL account lifecycle.
    #[account(constraint = spl_token_program.key() == SPL_TOKEN_PROGRAM_ID @ LaunchCtrlError::InvalidProgram)]
    pub spl_token_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}
