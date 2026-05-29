use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_spl::token_interface::spl_token_2022;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::TokenAccount;

use crate::constants::*;
use crate::errors::LaunchCtrlError;
use crate::events::MeteoraPoolCreated;
use crate::state::*;

/// Trustless, permissionless Meteora DAMM v2 pool creation.
///
/// Wraps the bonding curve SOL (held in migration_sol_vault) into WSOL,
/// then CPIs into DAMM v2's `initialize_customizable_pool` using
/// `migration_authority` PDA as the pool creator/signer. The LP position
/// NFT is minted to a PDA derived from our program — permanently locked
/// with no withdrawal instruction.
///
/// `init_sqrt_price`  — Q64.64 current price, computed off-chain by crank.
/// `liquidity_delta`  — Q64.64 liquidity amount, computed off-chain by crank.
/// `drip_total`       — withheld token amount at migration time (informational).
pub fn create_meteora_pool(
    ctx: Context<CreateMeteoraPool>,
    init_sqrt_price: u128,
    liquidity_delta: u128,
    drip_total: u64,
) -> Result<()> {
    let mint_key = ctx.accounts.mint.key();
    let mig_auth_bump = ctx.bumps.migration_authority;
    let position_nft_mint_bump = ctx.bumps.position_nft_mint;
    let wsol_vault_bump = ctx.bumps.wsol_vault;

    let migration_auth_seeds: &[&[u8]] =
        &[b"migration_authority", mint_key.as_ref(), &[mig_auth_bump]];
    let position_nft_mint_seeds: &[&[u8]] =
        &[b"position_nft_mint", mint_key.as_ref(), &[position_nft_mint_bump]];
    let wsol_vault_seeds: &[&[u8]] =
        &[b"wsol_vault", mint_key.as_ref(), &[wsol_vault_bump]];

    // ── 1. Compute amounts ─────────────────────────────────────────────────
    // migration_sol_vault holds sol_vault_rent + bonding-curve SOL. Most of
    // the bonded SOL is deposited into the pool; the rest reimburses the
    // cranker for migration costs.
    //
    // Sequencing:
    //   - bonded_sol = migration_sol_total - sol_vault_rent
    //   - sol_amount (LP deposit) = bonded_sol - CRANKER_MIGRATION_REIMBURSEMENT_LAMPORTS
    //   - At step 9, ALL lamports above sol_vault_rent in migration_sol_vault
    //     are drained to the cranker. The cranker fronts wsol_total =
    //     wsol_account_rent + sol_amount into wsol_vault; that wsol_account_rent
    //     comes back through wsol_vault's close at step 8, and the held-back
    //     CRANKER_MIGRATION_REIMBURSEMENT_LAMPORTS naturally flow to the
    //     cranker via the same step 9 drain. No new transfer step needed —
    //     it's just smaller LP deposit + bigger reimbursement.
    //
    // Why the held-back amount: the cranker pays ~0.026 SOL of stranded PDA
    // rent per migration (migration_authority + reward_vault + deployer_vault
    // + Meteora-side pool/position/vault rent + tx fees), which without
    // reimbursement bleeds the cranker wallet to zero over time. Carving 0.035
    // SOL from the bonded raise makes the cranker net +0.009 SOL per
    // migration — covers the cost + builds a buffer for priority fees,
    // rent rate increases, or Meteora layout drift. See
    // CRANKER_MIGRATION_REIMBURSEMENT_LAMPORTS in constants.rs for rationale.
    let migration_sol_total = ctx.accounts.migration_sol_vault.to_account_info().lamports();
    let sol_vault_rent    = Rent::get()?.minimum_balance(8);
    let wsol_account_rent = Rent::get()?.minimum_balance(165);
    // Reserve for Meteora pool/vault/position account creation costs. Paid by
    // cranker upfront (step 3); unused remainder is swept back to cranker at
    // step 10's PDA-signed transfer (leaving migration_authority at
    // rent-exempt minimum for future reinject CPIs).
    const METEORA_CREATION_RESERVE: u64 = 30_000_000; // 0.030 SOL (generous buffer)
    let bonded_sol = migration_sol_total
        .checked_sub(sol_vault_rent)
        .ok_or(LaunchCtrlError::InsufficientMigrationFunds)?;
    // Hold back the cranker reimbursement before computing the LP deposit.
    // Underflow here would mean the curve raised less than 0.035 SOL, which
    // is below MIN_MIGRATION_THRESHOLD_LAMPORTS (1 SOL) — but check anyway
    // so the program reverts cleanly instead of overflowing.
    let sol_amount = bonded_sol
        .checked_sub(CRANKER_MIGRATION_REIMBURSEMENT_LAMPORTS)
        .ok_or(LaunchCtrlError::InsufficientMigrationFunds)?;
    require!(sol_amount > 0, LaunchCtrlError::InsufficientMigrationFunds);

    let token_amount = ctx.accounts.migration_token_vault.amount;
    require!(token_amount > 0, LaunchCtrlError::InsufficientMigrationFunds);

    // ── 1b. Sanity-check init_sqrt_price (security audit M-1) ──────────────
    // create_meteora_pool is permissionless; without this check, an attacker
    // who front-runs the cranker can pass a degenerate sqrt_price and
    // permanently misprice the launch's pool. Reject any value that differs
    // from what the migration vault contents imply by more than ±10%.
    //
    // Meteora DAMM v2 stores price as sqrt(deposited_b / deposited_a) * 2^64
    // (Q64.64), where token_a / token_b are the byte-sorted mints. We approx
    // sqrt(b/a) * 2^64 as (isqrt(b) << 64) / isqrt(a). Each isqrt drops at
    // most 1 unit of precision; for our bounded inputs (sol_amount up to
    // u64::MAX lamports, token_amount up to ~10^15 raw units) the combined
    // relative error is well under 1e-5, orders of magnitude inside ±10%.
    let wsol_is_token_a = WSOL_MINT.to_bytes() < mint_key.to_bytes();
    let (deposited_a, deposited_b) = if wsol_is_token_a {
        (sol_amount as u128, token_amount as u128)
    } else {
        (token_amount as u128, sol_amount as u128)
    };
    let isqrt_a = deposited_a.isqrt();
    let isqrt_b = deposited_b.isqrt();
    require!(
        isqrt_a > 0 && isqrt_b > 0,
        LaunchCtrlError::InsufficientMigrationFunds
    );
    let expected_sqrt_price = (isqrt_b << 64)
        .checked_div(isqrt_a)
        .ok_or(LaunchCtrlError::MathOverflow)?;
    let tolerance = expected_sqrt_price / 10; // 10%
    let lower = expected_sqrt_price.saturating_sub(tolerance);
    let upper = expected_sqrt_price.saturating_add(tolerance);
    require!(
        init_sqrt_price >= lower && init_sqrt_price <= upper,
        LaunchCtrlError::InvalidInitSqrtPrice
    );

    // ── 2. Cranker creates + initializes WSOL vault ───────────────────────
    // Cranker (a system-owned signer) pays wsol_account_rent + sol_amount.
    // Using a real signer as payer avoids mutating the program-owned
    // migration_sol_vault lamports before any CPI runs.
    let wsol_total = wsol_account_rent
        .checked_add(sol_amount)
        .ok_or(LaunchCtrlError::InsufficientMigrationFunds)?;

    anchor_lang::solana_program::program::invoke_signed(
        &anchor_lang::solana_program::system_instruction::create_account(
            ctx.accounts.cranker.key,
            ctx.accounts.wsol_vault.key,
            wsol_total,
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

    // ── 3. Cranker funds migration_authority with METEORA_CREATION_RESERVE ─
    // migration_authority is the DAMM v2 payer for pool/position/vault rent.
    // Residual (unused reserve + recovered WSOL rent) is stranded here —
    // migration_authority is system-owned so we can't sweep it back.
    anchor_lang::solana_program::program::invoke(
        &anchor_lang::solana_program::system_instruction::transfer(
            ctx.accounts.cranker.key,
            ctx.accounts.migration_authority.key,
            METEORA_CREATION_RESERVE,
        ),
        &[
            ctx.accounts.cranker.to_account_info(),
            ctx.accounts.migration_authority.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
        ],
    )?;

    // ── 4. Sort mints: token_a < token_b (byte comparison) ────────────────
    // `wsol_is_token_a` was determined in step 1b for the sqrt-price check.
    let (token_a_mint_key, token_b_mint_key) = if wsol_is_token_a {
        (WSOL_MINT, mint_key)
    } else {
        (mint_key, WSOL_MINT)
    };

    let wsol_vault_info = ctx.accounts.wsol_vault.to_account_info();
    let migration_vault_info = ctx.accounts.migration_token_vault.to_account_info();
    let spl_prog_info = ctx.accounts.spl_token_program.to_account_info();
    let t22_prog_info = ctx.accounts.token_2022_program.to_account_info();
    let wsol_mint_info = ctx.accounts.wsol_mint.to_account_info();
    let our_mint_info = ctx.accounts.mint.to_account_info();

    let (payer_token_a, payer_token_b) = if wsol_is_token_a {
        (wsol_vault_info.clone(), migration_vault_info.clone())
    } else {
        (migration_vault_info.clone(), wsol_vault_info.clone())
    };
    let (token_a_prog, token_b_prog) = if wsol_is_token_a {
        (spl_prog_info.clone(), t22_prog_info.clone())
    } else {
        (t22_prog_info.clone(), spl_prog_info.clone())
    };
    let (token_a_mint_info, token_b_mint_info) = if wsol_is_token_a {
        (wsol_mint_info, our_mint_info)
    } else {
        (our_mint_info, wsol_mint_info)
    };

    // ── 5. Build initialize_customizable_pool instruction data ─────────────
    // Layout: [discriminator 8] [PoolFeeParameters 31] [sqrt_min_price 16]
    //         [sqrt_max_price 16] [has_alpha_vault 1] [liquidity 16]
    //         [sqrt_price 16] [activation_type 1] [collect_fee_mode 1]
    //         [activation_point Option<u64> 1]
    let mut init_data = DAMM_V2_DISC_INIT_POOL.to_vec();
    init_data.extend_from_slice(&POOL_FEE_PARAMS);
    init_data.extend_from_slice(&SQRT_MIN_PRICE.to_le_bytes());
    init_data.extend_from_slice(&SQRT_MAX_PRICE.to_le_bytes());
    init_data.push(0u8);                                     // has_alpha_vault = false
    init_data.extend_from_slice(&liquidity_delta.to_le_bytes());
    init_data.extend_from_slice(&init_sqrt_price.to_le_bytes());
    init_data.push(0u8);                                     // activation_type = 0 (slot)
    init_data.push(0u8);                                     // collect_fee_mode = 0 (BothToken) — avoids mint-sort dependency
    init_data.push(0u8);                                     // activation_point = None

    // ── 6. CPI: Meteora DAMM v2 initialize_customizable_pool ──────────────
    anchor_lang::solana_program::program::invoke_signed(
        &Instruction {
            program_id: METEORA_DAMM_V2_PROGRAM,
            accounts: vec![
                AccountMeta::new(ctx.accounts.migration_authority.key(), true),
                AccountMeta::new(*ctx.accounts.position_nft_mint.key, true),
                AccountMeta::new(ctx.accounts.position_nft_account.key(), false),
                AccountMeta::new(ctx.accounts.migration_authority.key(), true),
                AccountMeta::new_readonly(ctx.accounts.pool_authority.key(), false),
                AccountMeta::new(ctx.accounts.meteora_pool.key(), false),
                AccountMeta::new(ctx.accounts.meteora_position.key(), false),
                AccountMeta::new_readonly(token_a_mint_key, false),
                AccountMeta::new_readonly(token_b_mint_key, false),
                AccountMeta::new(ctx.accounts.token_a_vault.key(), false),
                AccountMeta::new(ctx.accounts.token_b_vault.key(), false),
                AccountMeta::new(payer_token_a.key(), false),
                AccountMeta::new(payer_token_b.key(), false),
                AccountMeta::new_readonly(token_a_prog.key(), false),
                AccountMeta::new_readonly(token_b_prog.key(), false),
                AccountMeta::new_readonly(ctx.accounts.token_2022_program.key(), false),
                AccountMeta::new_readonly(ctx.accounts.system_program.key(), false),
                AccountMeta::new_readonly(ctx.accounts.event_authority.key(), false),
                AccountMeta::new_readonly(ctx.accounts.meteora_program.key(), false),
            ],
            data: init_data,
        },
        &[
            ctx.accounts.migration_authority.to_account_info(),
            ctx.accounts.position_nft_mint.to_account_info(),
            ctx.accounts.position_nft_account.to_account_info(),
            ctx.accounts.migration_authority.to_account_info(),
            ctx.accounts.pool_authority.to_account_info(),
            ctx.accounts.meteora_pool.to_account_info(),
            ctx.accounts.meteora_position.to_account_info(),
            token_a_mint_info,
            token_b_mint_info,
            ctx.accounts.token_a_vault.to_account_info(),
            ctx.accounts.token_b_vault.to_account_info(),
            payer_token_a,
            payer_token_b,
            token_a_prog,
            token_b_prog,
            ctx.accounts.token_2022_program.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.event_authority.to_account_info(),
            ctx.accounts.meteora_program.to_account_info(),
        ],
        &[migration_auth_seeds, position_nft_mint_seeds],
    )?;

    // ── 6b. Sanity-check that meaningful liquidity was actually deposited ───
    // create_meteora_pool is permissionless. Without this gate, a malicious
    // cranker can pass an absurdly small `liquidity_delta` so the pool init
    // deposits next-to-nothing — leaving the bulk of `migration_token_vault`
    // stranded with no instruction to recover it AND a Meteora pool with
    // negligible locked liquidity. Reject any call that leaves more than
    // MAX_POOL_INIT_TOKEN_RESIDUAL_BPS of the original tokens behind.
    ctx.accounts.migration_token_vault.reload()?;
    let token_residual = ctx.accounts.migration_token_vault.amount;
    let max_residual = (token_amount as u128)
        .checked_mul(MAX_POOL_INIT_TOKEN_RESIDUAL_BPS as u128)
        .and_then(|v| v.checked_div(10_000u128))
        .ok_or(LaunchCtrlError::MathOverflow)? as u64;
    require!(
        token_residual <= max_residual,
        LaunchCtrlError::InsufficientPoolLiquidity
    );

    // ── 7. Permanently lock all liquidity in the position ─────────────────
    // Explicit on-chain lock visible in Meteora UI / explorers (Meteora's
    // `permanent_lock_position`). Without this the position is only
    // *implicitly* locked (no withdrawal instruction in our program). The
    // explicit call surfaces the lock status to indexers and trust scanners.
    //
    // Meteora PermanentLockPositionCtx accounts (in order):
    //   pool (mut), position (mut), position_nft_account, owner (signer),
    //   event_authority, program
    // Data: [disc 8] + [permanent_lock_liquidity u128 LE]  → 24 bytes
    {
        let mut lock_data = DAMM_V2_DISC_PERMANENT_LOCK.to_vec();
        lock_data.extend_from_slice(&liquidity_delta.to_le_bytes());

        anchor_lang::solana_program::program::invoke_signed(
            &Instruction {
                program_id: METEORA_DAMM_V2_PROGRAM,
                accounts: vec![
                    AccountMeta::new(ctx.accounts.meteora_pool.key(), false),
                    AccountMeta::new(ctx.accounts.meteora_position.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.position_nft_account.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.migration_authority.key(), true),
                    AccountMeta::new_readonly(ctx.accounts.event_authority.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.meteora_program.key(), false),
                ],
                data: lock_data,
            },
            &[
                ctx.accounts.meteora_pool.to_account_info(),
                ctx.accounts.meteora_position.to_account_info(),
                ctx.accounts.position_nft_account.to_account_info(),
                ctx.accounts.migration_authority.to_account_info(),
                ctx.accounts.event_authority.to_account_info(),
                ctx.accounts.meteora_program.to_account_info(),
            ],
            &[migration_auth_seeds],
        )?;
    }

    // ── 8. Close WSOL vault → return rent to migration_authority ─────────
    // SPL Token close_account: instruction index 9. authority = migration_authority.
    anchor_lang::solana_program::program::invoke_signed(
        &Instruction {
            program_id: SPL_TOKEN_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(*ctx.accounts.wsol_vault.key, false),
                AccountMeta::new(*ctx.accounts.migration_authority.key, false),
                AccountMeta::new_readonly(*ctx.accounts.migration_authority.key, true),
            ],
            data: vec![9u8],
        },
        &[
            ctx.accounts.wsol_vault.to_account_info(),
            ctx.accounts.migration_authority.to_account_info(),
            ctx.accounts.migration_authority.to_account_info(),
        ],
        &[migration_auth_seeds],
    )?;

    // ── 9. Sweep migration_authority residual back to cranker ───────────
    // migration_authority is system-owned (UncheckedAccount PDA), so we
    // can't raw-debit its lamports, but we CAN call system_program::transfer
    // with PDA-signing, which IS authorized. Reclaim the unused portion of
    // METEORA_CREATION_RESERVE + the WSOL rent recovered during step 8's
    // close_account, leaving just enough to stay rent-exempt so reinject's
    // future Meteora CPIs (which need migration_authority to PDA-sign)
    // don't break.
    //
    // Ordering: this CPI runs BEFORE step 10's raw lamport mutation so all
    // CPIs in the instruction complete before any account's lamports are
    // mutated raw. Lamport conservation is tracked per-CPI by the runtime;
    // doing the raw mutation last is the canonical safe pattern (matches
    // reinject.rs). Reversing the order trips Mollusk's conservation check
    // under unit test, even though both orders produce identical end state.
    let mig_auth_balance = ctx.accounts.migration_authority.to_account_info().lamports();
    let mig_auth_min_rent = Rent::get()?.minimum_balance(0);
    let mig_auth_sweep = mig_auth_balance.saturating_sub(mig_auth_min_rent);
    if mig_auth_sweep > 0 {
        anchor_lang::solana_program::program::invoke_signed(
            &anchor_lang::solana_program::system_instruction::transfer(
                ctx.accounts.migration_authority.key,
                ctx.accounts.cranker.key,
                mig_auth_sweep,
            ),
            &[
                ctx.accounts.migration_authority.to_account_info(),
                ctx.accounts.cranker.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
            &[migration_auth_seeds],
        )?;
    }

    // ── 10. Reimburse cranker for the SOL loaned into the pool ───────────
    // All CPIs complete. Raw lamport mutation on program-owned
    // migration_sol_vault is safe here. Drain everything above its
    // rent-exempt minimum back to cranker (covers sol_amount).
    let sol_vault_balance = ctx.accounts.migration_sol_vault.to_account_info().lamports();
    let reimburse = sol_vault_balance.saturating_sub(sol_vault_rent);
    if reimburse > 0 {
        **ctx.accounts.migration_sol_vault.to_account_info().try_borrow_mut_lamports()? -= reimburse;
        **ctx.accounts.cranker.to_account_info().try_borrow_mut_lamports()? += reimburse;
    }

    // ── 11. Record pool in launch_state ──────────────────────────────────
    let launch = &mut ctx.accounts.launch_state;
    launch.meteora_pool = ctx.accounts.meteora_pool.key();
    launch.drip_total = drip_total;

    emit!(MeteoraPoolCreated {
        mint: mint_key,
        meteora_pool: ctx.accounts.meteora_pool.key(),
        sol_deposited: sol_amount,
        tokens_deposited: token_amount,
        drip_total,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct CreateMeteoraPool<'info> {
    /// Permissionless cranker — pays WSOL vault creation rent + DAMM v2 account rent.
    #[account(mut)]
    pub cranker: Signer<'info>,

    /// CHECK: Token-2022 mint.
    #[account(
        constraint = mint.owner == &spl_token_2022::ID @ LaunchCtrlError::InvalidMintOwner
    )]
    pub mint: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_state.bump,
        constraint = launch_state.is_migrated @ LaunchCtrlError::NotMigrated,
        constraint = launch_state.meteora_pool == Pubkey::default() @ LaunchCtrlError::PoolAlreadySet,
    )]
    pub launch_state: Account<'info, LaunchState>,

    /// CHECK: PDA — signs as pool creator in DAMM v2 CPI.
    #[account(
        mut,
        seeds = [b"migration_authority", mint.key().as_ref()],
        bump,
    )]
    pub migration_authority: UncheckedAccount<'info>,

    /// Token-2022 vault — all tokens deposited into the DAMM v2 pool.
    #[account(
        mut,
        seeds = [b"migration_vault", mint.key().as_ref()],
        bump = launch_state.drip_vault_bump,
    )]
    pub migration_token_vault: InterfaceAccount<'info, TokenAccount>,

    /// SOL vault — holds bonding curve SOL for WSOL wrapping.
    #[account(
        mut,
        seeds = [b"migration_sol", mint.key().as_ref()],
        bump = launch_state.migration_sol_vault_bump,
    )]
    pub migration_sol_vault: Account<'info, MigrationSolVault>,

    /// Per-mint Diamond Hands rewards accumulator. Initialized here so every
    /// migrated token has a vault ready to receive the 25% LP-fee share on
    /// the first `claim_and_reinject` call (FEE_REWORK April 2026).
    #[account(
        init,
        payer = cranker,
        space = 8,
        seeds = [REWARD_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub reward_vault: Account<'info, RewardVault>,

    /// Per-mint Deployer Bonus accumulator (NEST_EGG_TO_DEV_BONUS Phase 3,
    /// replaces the legacy `nest_egg_vault`). Receives 6.25% of post-migration
    /// LP-fee shares when the creator still holds their initial buy. Init
    /// (not init_if_needed) — one-shot per pool creation, no reinit possible
    /// per the security checklist's reinitialization-attack mitigation.
    #[account(
        init,
        payer = cranker,
        space = 8,
        seeds = [DEPLOYER_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub deployer_vault: Account<'info, DeployerVault>,

    /// CHECK: Ephemeral WSOL account — created and closed within this instruction.
    #[account(
        mut,
        seeds = [b"wsol_vault", mint.key().as_ref()],
        bump,
    )]
    pub wsol_vault: UncheckedAccount<'info>,

    /// CHECK: WSOL native mint.
    #[account(constraint = wsol_mint.key() == WSOL_MINT @ LaunchCtrlError::InvalidProgram)]
    pub wsol_mint: UncheckedAccount<'info>,

    /// CHECK: Our PDA used as position_nft_mint in DAMM v2 — init + signer via invoke_signed.
    #[account(
        mut,
        seeds = [b"position_nft_mint", mint.key().as_ref()],
        bump,
    )]
    pub position_nft_mint: UncheckedAccount<'info>,

    // ─── DAMM v2 accounts (validated internally by Meteora) ──────────────────

    /// CHECK: Meteora DAMM v2 global pool authority PDA (seeds ["pool_authority"]).
    #[account(constraint = pool_authority.key() == METEORA_POOL_AUTHORITY @ LaunchCtrlError::InvalidProgram)]
    pub pool_authority: UncheckedAccount<'info>,

    /// CHECK: DAMM v2 pool PDA (seeds ["cpool", max_mint, min_mint] of DAMM v2 program).
    #[account(mut)]
    pub meteora_pool: UncheckedAccount<'info>,

    /// CHECK: DAMM v2 position PDA (seeds ["position", position_nft_mint] of DAMM v2 program).
    #[account(mut)]
    pub meteora_position: UncheckedAccount<'info>,

    /// CHECK: DAMM v2 position NFT account (seeds ["position_nft_account", position_nft_mint]).
    #[account(mut)]
    pub position_nft_account: UncheckedAccount<'info>,

    /// CHECK: DAMM v2 token A vault (seeds ["token_vault", token_a_mint, pool]).
    #[account(mut)]
    pub token_a_vault: UncheckedAccount<'info>,

    /// CHECK: DAMM v2 token B vault (seeds ["token_vault", token_b_mint, pool]).
    #[account(mut)]
    pub token_b_vault: UncheckedAccount<'info>,

    // ─── Programs ─────────────────────────────────────────────────────────────

    /// CHECK: Meteora DAMM v2 event authority PDA for CPI event logging (seeds ["__event_authority"]).
    #[account(constraint = event_authority.key() == METEORA_EVENT_AUTHORITY @ LaunchCtrlError::InvalidProgram)]
    pub event_authority: UncheckedAccount<'info>,

    /// CHECK: Must match METEORA_DAMM_V2_PROGRAM.
    #[account(constraint = meteora_program.key() == METEORA_DAMM_V2_PROGRAM @ LaunchCtrlError::InvalidProgram)]
    pub meteora_program: UncheckedAccount<'info>,

    pub token_2022_program: Program<'info, Token2022>,

    /// CHECK: Classic SPL Token program (for WSOL).
    #[account(constraint = spl_token_program.key() == SPL_TOKEN_PROGRAM_ID @ LaunchCtrlError::InvalidProgram)]
    pub spl_token_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}
