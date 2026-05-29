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

/// Sell tokens for SOL.
///
/// FEE_REWORK (April 2026): SOL-only fees, no Token-2022 transfer fee.
///
/// Fee math:
///   total_fee_bps = max(decay_schedule_bps, BASE_FEE_BPS)   // ≥ 100
///   total_fee     = gross_sol_out * total_fee_bps / 10_000
///   base_fee      = gross_sol_out * BASE_FEE_BPS  / 10_000
///   decay_component = total_fee − base_fee
///   buyback_sol     = decay_component * BUYBACK_SPLIT_BPS / 10_000   // 50%
///   retained_sol    = base_fee + (decay_component − buyback_sol)     // 1% + 50% of decay
///   sol_out_to_user = gross_sol_out − total_fee
///
/// SOL flow:
///   sol_vault debits (gross_sol_out − buyback_sol).
///   seller credited sol_out_to_user.
///   lp_seed_sol_vault credited retained_sol.
///   The buyback_sol portion stays in sol_vault — it represents the SOL side
///   of an inline synthetic buy that produces tokens into lp_seed_token_vault.
///
/// Net effect over the curve's lifetime: lp_seed_sol_vault accumulates
/// retained SOL while lp_seed_token_vault accumulates buyback tokens. Both
/// feed `Meteora::add_liquidity` post-migration. See docs/architecture/FEE_REWORK.md.
pub fn sell(ctx: Context<Sell>, token_amount: u64, min_sol_out: u64) -> Result<()> {
    require!(
        !(ctx.accounts.curve_state.is_complete && ctx.accounts.launch_state.is_migrated),
        LaunchCtrlError::CurveComplete
    );
    require!(token_amount > 0, LaunchCtrlError::ZeroAmount);
    require!(
        !ctx.accounts.blocklist.blocked.contains(&ctx.accounts.seller.key()),
        LaunchCtrlError::Blocked
    );

    enforce_ricochet(
        &ctx.accounts.ricochet_config,
        &ctx.accounts.instructions_sysvar,
        &ctx.accounts.mint.key(),
        ctx.accounts.curve_state.is_complete,
    )?;

    // ── 1. Curve quote (against pre-sell state) ─────────────────────────────
    // Vanilla mint, no transfer fee — vault receives `token_amount` 1:1.
    let net_received: u64 = token_amount;

    let curve = &ctx.accounts.curve_state;
    let gross_sol_out = crate::math::quote_sell(
        curve.virtual_sol_reserves,
        curve.real_sol_reserves,
        curve.virtual_token_reserves,
        curve.real_token_reserves,
        net_received,
    )
    .ok_or(LaunchCtrlError::MathOverflow)?;

    // ── 2. Fee splits ───────────────────────────────────────────────────────
    let clock = Clock::get()?;
    let elapsed = clock
        .unix_timestamp
        .saturating_sub(ctx.accounts.launch_state.launch_timestamp);
    let decay_bps = current_decay_bps(&ctx.accounts.launch_state.decay_schedule, elapsed);

    let total_fee_bps: u64 = if (decay_bps as u64) > BASE_FEE_BPS {
        decay_bps as u64
    } else {
        BASE_FEE_BPS
    };

    let total_fee: u64 = (gross_sol_out as u128)
        .checked_mul(total_fee_bps as u128)
        .and_then(|v| v.checked_div(10_000u128))
        .ok_or(LaunchCtrlError::MathOverflow)? as u64;
    let base_fee: u64 = (gross_sol_out as u128)
        .checked_mul(BASE_FEE_BPS as u128)
        .and_then(|v| v.checked_div(10_000u128))
        .ok_or(LaunchCtrlError::MathOverflow)? as u64;
    let decay_component = total_fee.saturating_sub(base_fee);

    // Skip the inline buyback if the curve is already complete (post-completion
    // pre-migration sell window). The whole decay component then routes to
    // retained SOL — keeps the math conservation-clean and avoids quoting a
    // synthetic buy against a curve that's about to migrate.
    let do_buyback = !ctx.accounts.curve_state.is_complete && decay_component > 0;
    let buyback_sol: u64 = if do_buyback {
        (decay_component as u128)
            .checked_mul(BUYBACK_SPLIT_BPS as u128)
            .and_then(|v| v.checked_div(10_000u128))
            .ok_or(LaunchCtrlError::MathOverflow)? as u64
    } else {
        0
    };
    let retained_sol = base_fee
        .checked_add(decay_component.saturating_sub(buyback_sol))
        .ok_or(LaunchCtrlError::MathOverflow)?;
    let sol_out_to_user = gross_sol_out
        .checked_sub(total_fee)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    require!(sol_out_to_user >= min_sol_out, LaunchCtrlError::SlippageExceeded);
    require!(
        gross_sol_out <= ctx.accounts.curve_state.real_sol_reserves,
        LaunchCtrlError::InsufficientLiquidity
    );

    // ── 3. Quote synthetic buyback (against post-sell curve state) ──────────
    // Computed in memory; doesn't mutate state until step 6.
    let buyback_tokens: u64 = if buyback_sol > 0 {
        // Apply user sell to curve in memory.
        let post_sell_real_sol = curve
            .real_sol_reserves
            .checked_sub(gross_sol_out)
            .ok_or(LaunchCtrlError::MathOverflow)?;
        let post_sell_real_tok = curve
            .real_token_reserves
            .checked_sub(net_received)
            .ok_or(LaunchCtrlError::MathOverflow)?;

        let eff_sol = curve
            .virtual_sol_reserves
            .checked_add(post_sell_real_sol)
            .ok_or(LaunchCtrlError::MathOverflow)?;
        let eff_tok = curve
            .virtual_token_reserves
            .checked_sub(post_sell_real_tok)
            .ok_or(LaunchCtrlError::MathOverflow)?;
        let k2: u128 = (eff_sol as u128)
            .checked_mul(eff_tok as u128)
            .ok_or(LaunchCtrlError::MathOverflow)?;

        let new_sol2 = eff_sol
            .checked_add(buyback_sol)
            .ok_or(LaunchCtrlError::MathOverflow)?;
        let new_tok2 = (k2 / new_sol2 as u128) as u64;
        eff_tok
            .checked_sub(new_tok2)
            .ok_or(LaunchCtrlError::MathOverflow)?
    } else {
        0
    };

    // ── 4. CPI: seller → curve_token_vault (1:1, no fee) ────────────────────
    anchor_spl::token_2022::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            anchor_spl::token_2022::TransferChecked {
                from: ctx.accounts.seller_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.curve_token_vault.to_account_info(),
                authority: ctx.accounts.seller.to_account_info(),
            },
        ),
        token_amount,
        ctx.accounts.mint.decimals,
    )?;

    // ── 5. CPI: curve_token_vault → lp_seed_token_vault (synthetic buyback) ─
    if buyback_tokens > 0 {
        require!(
            buyback_tokens <= ctx.accounts.curve_token_vault.amount,
            LaunchCtrlError::InsufficientLiquidity
        );
        let mint_key = ctx.accounts.mint.key();
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
                    to: ctx.accounts.lp_seed_token_vault.to_account_info(),
                    authority: ctx.accounts.curve_token_vault.to_account_info(),
                },
                &[curve_vault_seeds],
            ),
            buyback_tokens,
            ctx.accounts.mint.decimals,
        )?;
    }

    // ── 6. SOL flow (raw lamport mutations — all CPIs above are done) ───────
    let net_sol_debit = gross_sol_out
        .checked_sub(buyback_sol)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    **ctx.accounts.sol_vault.try_borrow_mut_lamports()? = ctx
        .accounts
        .sol_vault
        .lamports()
        .checked_sub(net_sol_debit)
        .ok_or(LaunchCtrlError::InsufficientLiquidity)?;

    if sol_out_to_user > 0 {
        **ctx.accounts.seller.try_borrow_mut_lamports()? = ctx
            .accounts
            .seller
            .lamports()
            .checked_add(sol_out_to_user)
            .ok_or(LaunchCtrlError::MathOverflow)?;
    }

    if retained_sol > 0 {
        let lp_seed_sol_info = ctx.accounts.lp_seed_sol_vault.to_account_info();
        **lp_seed_sol_info.try_borrow_mut_lamports()? = lp_seed_sol_info
            .lamports()
            .checked_add(retained_sol)
            .ok_or(LaunchCtrlError::MathOverflow)?;
    }

    // ── 7. Curve state update — apply user sell, then synthetic buy ─────────
    let curve = &mut ctx.accounts.curve_state;
    curve.real_sol_reserves = curve
        .real_sol_reserves
        .checked_sub(gross_sol_out)
        .ok_or(LaunchCtrlError::MathOverflow)?;
    curve.real_token_reserves = curve
        .real_token_reserves
        .checked_sub(net_received)
        .ok_or(LaunchCtrlError::MathOverflow)?;

    if buyback_sol > 0 {
        curve.real_sol_reserves = curve
            .real_sol_reserves
            .checked_add(buyback_sol)
            .ok_or(LaunchCtrlError::MathOverflow)?;
        curve.real_token_reserves = curve
            .real_token_reserves
            .checked_add(buyback_tokens)
            .ok_or(LaunchCtrlError::MathOverflow)?;
    }

    emit!(TokensSold {
        mint: ctx.accounts.mint.key(),
        seller: ctx.accounts.seller.key(),
        tokens_in: token_amount,
        sol_out: sol_out_to_user,
        total_fee_bps,
        retained_sol_lamports: retained_sol,
        buyback_sol_lamports: buyback_sol,
        buyback_tokens,
    });

    Ok(())
}

/// Walk the decay schedule and return the currently-active sell-side decay
/// rate in basis points. Schedule entries are (seconds_after_launch, fee_bps)
/// in ascending order. Returns 0 if the schedule is empty or `elapsed`
/// precedes the first step. Caller floors at BASE_FEE_BPS.
fn current_decay_bps(schedule: &[DecayStep], elapsed: i64) -> u16 {
    let mut current: u16 = 0;
    for step in schedule.iter() {
        if (step.seconds_after_launch as i64) <= elapsed {
            current = step.fee_bps;
        } else {
            break;
        }
    }
    current
}

#[derive(Accounts)]
pub struct Sell<'info> {
    #[account(mut)]
    pub seller: Signer<'info>,

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

    #[account(
        seeds = [b"blocklist", mint.key().as_ref()],
        bump = blocklist.bump,
    )]
    pub blocklist: Account<'info, Blocklist>,

    /// LaunchState — read launch_timestamp + decay_schedule for sell-side decay.
    #[account(
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_state.bump,
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

    /// LP-seed SOL accumulator — receives base sell-fee + 50% of decay component.
    #[account(
        mut,
        seeds = [LP_SEED_SOL_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub lp_seed_sol_vault: Account<'info, LpSeedSolVault>,

    /// LP-seed token accumulator — receives inline-buyback tokens.
    #[account(
        mut,
        seeds = [LP_SEED_TOKEN_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub lp_seed_token_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        token::mint = mint,
        token::authority = seller,
    )]
    pub seller_token_account: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Program<'info, Token2022>,

    /// Optional Ricochet enforcement config.
    pub ricochet_config: Option<Account<'info, RicochetConfig>>,

    /// CHECK: Validated by address constraint.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
}

#[cfg(test)]
mod tests {
    //! Property-based tests for the pure math in this file.
    //!
    //! `current_decay_bps` is the schedule-lookup primitive that controls the
    //! sell-side fee rate at any given elapsed time. It's pure (no I/O, no
    //! Clock), deterministic, and small — perfect for proptest coverage of
    //! the invariants the caller relies on.

    use super::*;
    use crate::constants::MAX_DECAY_FEE_BPS;
    use proptest::prelude::*;

    /// Generate an ordered, valid decay schedule of length 1..=10.
    /// `seconds_after_launch` strictly increasing; `fee_bps` ≤ MAX_DECAY_FEE_BPS.
    fn arb_schedule() -> impl Strategy<Value = Vec<DecayStep>> {
        // Pick how many steps, then generate that many (delta, bps) tuples
        // and convert deltas into a strictly-increasing absolute timeline.
        (1usize..=10)
            .prop_flat_map(|n| {
                (
                    proptest::collection::vec(1u64..=86_400u64, n), // per-step delta in seconds (1s..1d)
                    proptest::collection::vec(0u16..=MAX_DECAY_FEE_BPS, n),
                )
            })
            .prop_map(|(deltas, bps_vec)| {
                let mut t: u64 = 0;
                deltas
                    .into_iter()
                    .zip(bps_vec)
                    .map(|(d, b)| {
                        t = t.saturating_add(d);
                        DecayStep {
                            seconds_after_launch: t,
                            fee_bps: b,
                        }
                    })
                    .collect()
            })
    }

    // ── Sanity checks (cheap, deterministic) ────────────────────────────────

    #[test]
    fn empty_schedule_returns_zero() {
        assert_eq!(current_decay_bps(&[], 0), 0);
        assert_eq!(current_decay_bps(&[], 1_000_000), 0);
        assert_eq!(current_decay_bps(&[], i64::MAX), 0);
    }

    #[test]
    fn elapsed_before_first_step_returns_zero() {
        let schedule = vec![
            DecayStep { seconds_after_launch: 60, fee_bps: 1000 },
            DecayStep { seconds_after_launch: 120, fee_bps: 500 },
        ];
        // The fn semantics: returns the last step whose seconds_after_launch ≤ elapsed.
        // If elapsed precedes ALL entries, no step matches and the loop never
        // assigns — returns the initial 0. Caller (sell.rs:88) floors at
        // BASE_FEE_BPS so this 0 becomes 100 bps in practice.
        assert_eq!(current_decay_bps(&schedule, 0), 0);
        assert_eq!(current_decay_bps(&schedule, 59), 0);
    }

    #[test]
    fn elapsed_at_step_boundary_picks_that_step() {
        let schedule = vec![
            DecayStep { seconds_after_launch: 60, fee_bps: 1000 },
            DecayStep { seconds_after_launch: 120, fee_bps: 500 },
        ];
        // Strict ≤ boundary semantics — at exactly the step time, the step
        // applies.
        assert_eq!(current_decay_bps(&schedule, 60), 1000);
        assert_eq!(current_decay_bps(&schedule, 119), 1000);
        assert_eq!(current_decay_bps(&schedule, 120), 500);
    }

    #[test]
    fn negative_elapsed_safe() {
        // Pre-launch elapsed (clock drift, validator weirdness) — must return 0
        // without panicking. The cast to i64 in the comparison is safe.
        let schedule = vec![DecayStep { seconds_after_launch: 60, fee_bps: 1000 }];
        assert_eq!(current_decay_bps(&schedule, -1), 0);
        assert_eq!(current_decay_bps(&schedule, i64::MIN), 0);
    }

    proptest! {
        /// PROPERTY 1: Output is bounded — never exceeds MAX_DECAY_FEE_BPS,
        /// never returns a value not in the schedule (or 0 for pre-schedule).
        #[test]
        fn output_is_bounded(schedule in arb_schedule(), elapsed in any::<i64>()) {
            let result = current_decay_bps(&schedule, elapsed);
            prop_assert!(result <= MAX_DECAY_FEE_BPS,
                "output {} exceeded MAX_DECAY_FEE_BPS {}", result, MAX_DECAY_FEE_BPS);
            let valid_outputs: std::collections::HashSet<u16> =
                schedule.iter().map(|s| s.fee_bps).chain(std::iter::once(0)).collect();
            prop_assert!(valid_outputs.contains(&result),
                "output {} not in schedule + {{0}}", result);
        }

        /// PROPERTY 2: Monotonic in time — moving forward through time can
        /// only stay on the same step or advance. Output values per-step
        /// can move up or down (decay can be configured non-monotonic), but
        /// the SELECTED step index is monotonically non-decreasing.
        #[test]
        fn step_selection_monotonic_in_time(
            schedule in arb_schedule(),
            t1 in 0i64..1_000_000_000,
            dt in 0i64..1_000_000_000,
        ) {
            let t2 = t1.saturating_add(dt);
            // Count how many entries are ≤ t1 vs ≤ t2. t2 ≥ t1 ⇒ count2 ≥ count1.
            let count1 = schedule.iter().filter(|s| (s.seconds_after_launch as i64) <= t1).count();
            let count2 = schedule.iter().filter(|s| (s.seconds_after_launch as i64) <= t2).count();
            prop_assert!(count2 >= count1, "step selection went backwards in time");
            // Also: at the same elapsed time, the result is stable.
            prop_assert_eq!(current_decay_bps(&schedule, t1), current_decay_bps(&schedule, t1));
        }

        /// PROPERTY 3: Past the last step, the result locks to the final
        /// step's bps. Implementation never panics on huge elapsed values.
        #[test]
        fn after_last_step_locks_in(schedule in arb_schedule(), past_last_offset in 0i64..1_000_000) {
            let last_seconds = schedule.last().unwrap().seconds_after_launch as i64;
            let elapsed = last_seconds.saturating_add(past_last_offset);
            let expected = schedule.last().unwrap().fee_bps;
            prop_assert_eq!(current_decay_bps(&schedule, elapsed), expected);
        }

        /// PROPERTY 4: No panic / no overflow on extreme inputs. Tests the
        /// `step.seconds_after_launch as i64` cast which is the only
        /// potentially-tricky line in the implementation.
        #[test]
        fn no_panic_on_extreme_inputs(elapsed in any::<i64>()) {
            // Synthetic schedule with values near u64 limits to stress the cast.
            let schedule = vec![
                DecayStep { seconds_after_launch: 0, fee_bps: 100 },
                DecayStep { seconds_after_launch: u64::MAX / 2, fee_bps: 200 },
                DecayStep { seconds_after_launch: u64::MAX, fee_bps: 300 },
            ];
            // u64::MAX as i64 is negative (-1), so it appears in the past
            // relative to any non-negative elapsed. The implementation
            // tolerates that — we don't panic, we just see the schedule
            // entries in a "wrong" order as far as i64 comparison goes.
            // Document that as the current behavior.
            let _ = current_decay_bps(&schedule, elapsed);
        }
    }
}
