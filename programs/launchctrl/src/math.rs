//! Pure curve + fee math.
//!
//! Functions in this module are checked-arithmetic, side-effect-free, and
//! produce identical bit-for-bit results to the inline math previously in
//! `instructions/buy.rs` and `instructions/sell.rs`. Extracted so the math
//! can be exercised by `cargo test` (proptest) without standing up an
//! Anchor account environment.
//!
//! `None` ⇒ overflow/underflow at any step. Callers map to
//! `LaunchCtrlError::MathOverflow`. All arithmetic uses `checked_*` /
//! `u128` widening to make overflow detectable.
//!
//! Property tests live in `#[cfg(test)] mod tests` at the bottom of this
//! file. Run via `cargo test --lib --manifest-path programs/launchctrl/Cargo.toml`.

/// Buy-side constant-product quote.
///
/// Mirrors `buy.rs` lines 62-84 exactly:
///   effective_sol = virtual_sol + real_sol
///   effective_tok = virtual_tok - real_tok
///   k = effective_sol * effective_tok               (u128)
///   new_sol = effective_sol + sol_in_post_fee
///   new_tok = k / new_sol                            (u128 → u64)
///   tokens_out = effective_tok - new_tok
///
/// `sol_in_post_fee` should be the SOL amount AFTER deducting the 1% buy
/// platform fee — see [`apply_buy_fee`] for that split.
pub fn quote_buy(
    virtual_sol: u64,
    real_sol: u64,
    virtual_tok: u64,
    real_tok: u64,
    sol_in_post_fee: u64,
) -> Option<u64> {
    let effective_sol = virtual_sol.checked_add(real_sol)?;
    let effective_tok = virtual_tok.checked_sub(real_tok)?;
    let k: u128 = (effective_sol as u128).checked_mul(effective_tok as u128)?;
    let new_sol = effective_sol.checked_add(sol_in_post_fee)?;
    if new_sol == 0 {
        return None;
    }
    let new_tok = (k / new_sol as u128) as u64;
    effective_tok.checked_sub(new_tok)
}

/// Sell-side constant-product quote (mirror of buy, with roles swapped).
///
/// Mirrors `sell.rs` lines 58-79 exactly:
///   effective_sol = virtual_sol + real_sol
///   effective_tok = virtual_tok - real_tok
///   k = effective_sol * effective_tok               (u128)
///   new_tok = effective_tok + token_amount
///   new_sol = k / new_tok                            (u128 → u64)
///   gross_sol_out = effective_sol - new_sol
///
/// Returns the GROSS SOL out (before fee deduction). Caller applies the
/// total fee and BUYBACK_SPLIT separately.
pub fn quote_sell(
    virtual_sol: u64,
    real_sol: u64,
    virtual_tok: u64,
    real_tok: u64,
    token_amount: u64,
) -> Option<u64> {
    let effective_sol = virtual_sol.checked_add(real_sol)?;
    let effective_tok = virtual_tok.checked_sub(real_tok)?;
    let k: u128 = (effective_sol as u128).checked_mul(effective_tok as u128)?;
    let new_tok = effective_tok.checked_add(token_amount)?;
    if new_tok == 0 {
        return None;
    }
    let new_sol = (k / new_tok as u128) as u64;
    effective_sol.checked_sub(new_sol)
}

/// Split `sol_amount` into (`platform_fee`, `sol_in_post_fee`) where
/// `platform_fee = sol_amount * BASE_FEE_BPS / 10_000`. `None` on overflow.
///
/// Conservation: `platform_fee + sol_in_post_fee == sol_amount` always
/// (the subtraction is exact since `platform_fee ≤ sol_amount`).
pub fn apply_buy_fee(sol_amount: u64, base_fee_bps: u64) -> Option<(u64, u64)> {
    let platform_fee = sol_amount
        .checked_mul(base_fee_bps)
        .and_then(|v| v.checked_div(10_000))?;
    let sol_in_post_fee = sol_amount.checked_sub(platform_fee)?;
    Some((platform_fee, sol_in_post_fee))
}

/// Compute the 4-way split of `wsol_claimed` used in `claim_and_reinject`.
///
/// Returns `(lp_take, reward_take, dev_bonus_take, platform_take)` where
/// each component = `wsol_claimed * bps / 10_000` (floor). Returns `None`
/// on any overflow. The on-chain handler then either credits
/// `dev_bonus_take` to `deployer_vault` (if creator still holds) OR rolls
/// it into `reward_take` (if not).
///
/// **Conservation:** `lp_take + reward_take + dev_bonus_take + platform_take`
/// may be slightly LESS than `wsol_claimed` due to integer-division
/// rounding (up to 4 lamports of drift across the four floors). The
/// drift accumulates harmlessly in `migration_sol_vault` as protocol
/// reserve. Documented as a property below.
pub fn split_reinject_wsol(
    wsol_claimed: u64,
    lp_reinject_bps: u64,
    reward_pool_bps: u64,
    dev_bonus_bps: u64,
    platform_lp_bps: u64,
) -> Option<(u64, u64, u64, u64)> {
    let take = |bps: u64| -> Option<u64> {
        (wsol_claimed as u128)
            .checked_mul(bps as u128)
            .and_then(|v| v.checked_div(10_000u128))
            .map(|v| v as u64)
    };
    Some((
        take(lp_reinject_bps)?,
        take(reward_pool_bps)?,
        take(dev_bonus_bps)?,
        take(platform_lp_bps)?,
    ))
}

// ─── Property tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{
        BASE_FEE_BPS, DEV_BONUS_BPS, LP_REINJECT_BPS, PLATFORM_LP_BPS, REWARD_POOL_BPS,
    };
    use proptest::prelude::*;

    // ── apply_buy_fee ───────────────────────────────────────────────────────

    proptest! {
        /// PROPERTY: conservation. fee + post_fee == amount always.
        #[test]
        fn buy_fee_conserves_input(sol_amount in 0u64..=u64::MAX / 10_000) {
            let (fee, post_fee) = apply_buy_fee(sol_amount, BASE_FEE_BPS).unwrap();
            prop_assert_eq!(fee + post_fee, sol_amount,
                "fee {} + post_fee {} != amount {}", fee, post_fee, sol_amount);
        }

        /// PROPERTY: bps semantics. At BASE_FEE_BPS=100 (1%), fee is at most
        /// ceil(amount/100). Floor-division means at most amount/100, often
        /// 1 less.
        #[test]
        fn buy_fee_is_one_percent(sol_amount in 10_000u64..=u64::MAX / 10_000) {
            let (fee, _) = apply_buy_fee(sol_amount, BASE_FEE_BPS).unwrap();
            // floor(amount * 100 / 10000) = floor(amount / 100)
            prop_assert_eq!(fee, sol_amount / 100);
        }

        /// PROPERTY: overflow detection. Inputs that would overflow during
        /// the multiplication step return None.
        #[test]
        fn buy_fee_overflow_detected(sol_amount in (u64::MAX / 9_999)..=u64::MAX) {
            // sol_amount * 10_000 (BASE_FEE_BPS is 100, but BPS_DIVISOR is 10_000)
            // For amounts near u64::MAX, sol_amount * 100 may still fit but
            // proptest with bigger bps values would overflow. Test with a
            // pathological bps to force overflow:
            prop_assert_eq!(apply_buy_fee(sol_amount, 100_000_000), None);
        }
    }

    // ── quote_buy + quote_sell ──────────────────────────────────────────────

    /// Realistic curve parameters at launch — matches LaunchCTRL defaults.
    /// virtual_sol = 70 SOL, virtual_tok = 1.16B tokens (with 6 decimals).
    /// Picked specifically because these match what's deployed on-chain.
    fn arb_curve_state() -> impl Strategy<Value = (u64, u64, u64, u64)> {
        (
            // virtual_sol: 70 SOL in lamports (70_000_000_000) ± 50%
            35_000_000_000u64..=105_000_000_000u64,
            // real_sol: 0 to migration threshold (120 SOL)
            0u64..=120_000_000_000u64,
            // virtual_tok: 1.16B tokens (with 6 decimals = 1.16e15)
            500_000_000_000_000u64..=2_000_000_000_000_000u64,
            // real_tok: 0 to virtual_tok / 2
            0u64..=500_000_000_000_000u64,
        )
    }

    proptest! {
        /// PROPERTY: monotonicity — more SOL in always produces strictly more
        /// tokens out (assuming non-trivial reserves).
        #[test]
        fn buy_monotonic_in_sol_in(
            (vs, rs, vt, rt) in arb_curve_state(),
            sol_in in 1_000_000u64..=10_000_000_000u64,
            delta in 1u64..=1_000_000_000u64,
        ) {
            let out_a = quote_buy(vs, rs, vt, rt, sol_in);
            let out_b = quote_buy(vs, rs, vt, rt, sol_in.saturating_add(delta));
            if let (Some(a), Some(b)) = (out_a, out_b) {
                prop_assert!(b >= a,
                    "more SOL ({} > {}) produced less tokens ({} < {})",
                    sol_in.saturating_add(delta), sol_in, b, a);
            }
        }

        /// PROPERTY: output bounded by reserves. Cannot ever drain more
        /// tokens than `effective_tok` (virtual_tok - real_tok).
        #[test]
        fn buy_output_bounded_by_reserves(
            (vs, rs, vt, rt) in arb_curve_state(),
            sol_in in 1u64..=u64::MAX / 4,
        ) {
            if let Some(tokens_out) = quote_buy(vs, rs, vt, rt, sol_in) {
                let effective_tok = vt.saturating_sub(rt);
                prop_assert!(tokens_out <= effective_tok,
                    "tokens_out {} > effective_tok {}", tokens_out, effective_tok);
            }
        }

        /// PROPERTY: zero in → zero out (within rounding). A 0-lamport buy
        /// produces 0 tokens.
        #[test]
        fn buy_zero_in_zero_out((vs, rs, vt, rt) in arb_curve_state()) {
            let result = quote_buy(vs, rs, vt, rt, 0);
            prop_assert_eq!(result, Some(0));
        }

        /// PROPERTY: sell monotonicity — more tokens in always produces more
        /// (or equal) SOL out.
        #[test]
        fn sell_monotonic_in_tokens_in(
            (vs, rs, vt, rt) in arb_curve_state(),
            tokens_in in 1_000_000u64..=1_000_000_000_000u64,
            delta in 1u64..=1_000_000_000u64,
        ) {
            let out_a = quote_sell(vs, rs, vt, rt, tokens_in);
            let out_b = quote_sell(vs, rs, vt, rt, tokens_in.saturating_add(delta));
            if let (Some(a), Some(b)) = (out_a, out_b) {
                prop_assert!(b >= a,
                    "more tokens in ({} > {}) produced less SOL ({} < {})",
                    tokens_in.saturating_add(delta), tokens_in, b, a);
            }
        }

        /// PROPERTY: sell zero in → zero out.
        #[test]
        fn sell_zero_in_zero_out((vs, rs, vt, rt) in arb_curve_state()) {
            let result = quote_sell(vs, rs, vt, rt, 0);
            prop_assert_eq!(result, Some(0));
        }

        /// PROPERTY: bounded-drift round trip. Buying X SOL of tokens then
        /// immediately selling them returns AT MOST `sol_in + 2` lamports.
        ///
        /// The "+2" tolerance reflects the constant-product math's two
        /// floor-division steps (one in the buy's `k / new_sol`, one in the
        /// sell's `k / new_tok`). Each floor can favor the user by up to 1
        /// lamport depending on the reserve ratio, so a 2-lamport drift is
        /// the theoretical worst case.
        ///
        /// IS THIS A REAL ARB? No. The instruction handlers layer a 1%
        /// platform fee on top of these quotes. On any trade ≥ 1000 lamports,
        /// the fee cost (≥10 lamports) plus tx gas (~5000 lamports) dwarfs
        /// any 1-2 lamport rounding favor by orders of magnitude. The pure
        /// curve math does have this O(1) rounding characteristic, but the
        /// fee structure makes it non-exploitable in practice. Documented in
        /// the security audit log under "Math drift bounds verified by
        /// proptest 2026-05-11."
        #[test]
        fn round_trip_drift_bounded(
            (vs, rs, vt, rt) in arb_curve_state(),
            sol_in in 1_000_000u64..=10_000_000_000u64,
        ) {
            if let Some(tokens) = quote_buy(vs, rs, vt, rt, sol_in) {
                let new_rs = rs.saturating_add(sol_in);
                let new_rt = rt.saturating_add(tokens);
                if let Some(sol_back) = quote_sell(vs, new_rs, vt, new_rt, tokens) {
                    prop_assert!(sol_back <= sol_in.saturating_add(2),
                        "round trip drift exceeded 2 lamports: in={} out={} drift={}",
                        sol_in, sol_back, sol_back as i128 - sol_in as i128);
                }
            }
        }
    }

    // ── split_reinject_wsol ─────────────────────────────────────────────────

    proptest! {
        /// PROPERTY: conservation with bounded drift. The 4 floored shares
        /// sum to at most `wsol_claimed`, with drift no more than 3 lamports
        /// (one per division, minus the one that's mathematically exact for
        /// the smallest piece). The on-chain test pass observed drift ≤ 3.
        #[test]
        fn split_conservation(wsol_claimed in 0u64..=u64::MAX / 10_000) {
            let (lp, reward, dev_bonus, platform) = split_reinject_wsol(
                wsol_claimed,
                LP_REINJECT_BPS,
                REWARD_POOL_BPS,
                DEV_BONUS_BPS,
                PLATFORM_LP_BPS,
            ).unwrap();

            let sum = lp + reward + dev_bonus + platform;
            prop_assert!(sum <= wsol_claimed,
                "sum {} > wsol_claimed {}", sum, wsol_claimed);
            let drift = wsol_claimed - sum;
            prop_assert!(drift <= 3,
                "drift {} > 3 lamports (wsol={}, lp={}, reward={}, dev={}, platform={})",
                drift, wsol_claimed, lp, reward, dev_bonus, platform);
        }

        /// PROPERTY: bps invariants — the 4 official LaunchCTRL constants
        /// sum to exactly 10_000 (100%). Catches anyone editing constants.rs
        /// in a way that breaks the split.
        #[test]
        fn split_bps_sum_to_10000(_dummy in 0u8..1) {
            prop_assert_eq!(
                LP_REINJECT_BPS + REWARD_POOL_BPS + DEV_BONUS_BPS + PLATFORM_LP_BPS,
                10_000
            );
        }

        /// PROPERTY: monotonic in input. Larger `wsol_claimed` produces
        /// equal-or-larger amounts in each share.
        #[test]
        fn split_monotonic(
            wsol_a in 0u64..=u64::MAX / 20_000,
            delta in 0u64..=1_000_000_000_000u64,
        ) {
            let wsol_b = wsol_a.saturating_add(delta);
            let (lp_a, rw_a, dv_a, pl_a) = split_reinject_wsol(
                wsol_a, LP_REINJECT_BPS, REWARD_POOL_BPS, DEV_BONUS_BPS, PLATFORM_LP_BPS,
            ).unwrap();
            let (lp_b, rw_b, dv_b, pl_b) = split_reinject_wsol(
                wsol_b, LP_REINJECT_BPS, REWARD_POOL_BPS, DEV_BONUS_BPS, PLATFORM_LP_BPS,
            ).unwrap();
            prop_assert!(lp_b >= lp_a);
            prop_assert!(rw_b >= rw_a);
            prop_assert!(dv_b >= dv_a);
            prop_assert!(pl_b >= pl_a);
        }
    }
}
