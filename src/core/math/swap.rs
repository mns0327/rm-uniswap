//! # SwapMath — Optimized single-tick swap step computation
//!
//! Production-focused Rust port of Uniswap concentrated-liquidity swap-step math.
//!
//! Main optimizations:
//! - No tracing in the hot arithmetic path.
//! - No generic FullMath for fee arithmetic.
//! - Fee math uses exact U256 × u32 / u32 long division.
//! - Fee complement is computed once and passed down.
//! - Exact-input partial-fill accounting preserves Uniswap-style semantics:
//!   if target is not reached, amount_in is the full fee-adjusted input.

use alloy::primitives::I256;
use ruint::aliases::U256;

use crate::core::types::fee::{Fee, MAX_FEE};
use crate::core::types::nonzero::NonZeroLiquidity;
use crate::core::types::sqrt_price::SqrtPriceX96;

use super::full::MathError;
use super::small_ratio::{mul_div_u32_ceil, mul_div_u32_floor};
use super::sqrt_price::{
    get_amount0_delta, get_amount0_delta_with_liquidity_delta, get_amount1_delta,
    get_amount1_delta_with_liquidity_delta, get_next_sqrt_price_from_input,
    get_next_sqrt_price_from_output,
};

/// Backward-compatible name for the crate-wide compact error code.
pub type SwapMathError = crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapStep {
    pub sqrt_ratio_next_x96: SqrtPriceX96,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

#[inline(always)]
pub(crate) fn amount_less_fee_exact_in(
    amount_remaining: U256,
    fee: Fee,
) -> Result<U256, MathError> {
    if fee.is_zero() {
        return Ok(amount_remaining);
    }

    if fee.is_max() {
        return Ok(U256::ZERO);
    }

    mul_div_u32_floor(amount_remaining, fee.complement(), MAX_FEE)
}

#[inline(always)]
pub(crate) fn fee_on_exact_input(amount_in: U256, fee: Fee) -> Result<U256, MathError> {
    if amount_in.is_zero() || fee.is_zero() {
        return Ok(U256::ZERO);
    }

    if fee.is_max() {
        return Ok(amount_in);
    }

    // fee = ceil(amount_in * fee_pips / (MAX_SWAP_FEE - fee_pips))
    mul_div_u32_ceil(amount_in, fee.pips(), fee.complement())
}

#[inline(always)]
pub fn get_sqrt_price_target(
    zero_for_one: bool,
    sqrt_price_next_x96: SqrtPriceX96,
    sqrt_price_limit_x96: SqrtPriceX96,
) -> SqrtPriceX96 {
    if zero_for_one {
        sqrt_price_next_x96.max(sqrt_price_limit_x96)
    } else {
        sqrt_price_next_x96.min(sqrt_price_limit_x96)
    }
}

#[inline]
#[must_use = "discarding a swap step result silently drops errors"]
pub fn compute_swap_step(
    sqrt_ratio_current_x96: SqrtPriceX96,
    sqrt_ratio_target_x96: SqrtPriceX96,
    liquidity: NonZeroLiquidity,
    amount: I256,
    fee: Fee,
) -> Result<SwapStep, SwapMathError> {
    // Keep original behavior: fee is validated before zero-amount no-op.
    if amount.is_zero() {
        return Ok(SwapStep {
            sqrt_ratio_next_x96: sqrt_ratio_current_x96,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            fee_amount: U256::ZERO,
        });
    }

    let exact_in = amount.is_negative();

    if !exact_in && fee.is_max() {
        return Err(SwapMathError::MaxFeeExactOut);
    }

    let zero_for_one = sqrt_ratio_current_x96 >= sqrt_ratio_target_x96;
    let amount_remaining = amount.unsigned_abs();

    if exact_in {
        compute_exact_in(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            amount_remaining,
            fee,
            zero_for_one,
        )
    } else {
        compute_exact_out(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            amount_remaining,
            fee,
            zero_for_one,
        )
    }
}

#[inline]
fn compute_exact_in(
    sqrt_ratio_current_x96: SqrtPriceX96,
    sqrt_ratio_target_x96: SqrtPriceX96,
    liquidity: NonZeroLiquidity,
    amount_remaining: U256,
    fee: Fee,
    zero_for_one: bool,
) -> Result<SwapStep, SwapMathError> {
    let amount_remaining_less_fee = amount_less_fee_exact_in(amount_remaining, fee)?;

    let (max_amount_in, liquidity_delta_sqrt) = if zero_for_one {
        get_amount0_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity.unwrap(),
            true,
        )?
    } else {
        get_amount1_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity.unwrap(),
            true,
        )?
    };

    let reached_target = amount_remaining_less_fee >= max_amount_in;

    let (sqrt_ratio_next_x96, amount_in) = if reached_target {
        (sqrt_ratio_target_x96, max_amount_in)
    } else {
        // Uniswap-compatible accounting:
        // In a partial exact-input fill, consume the full fee-adjusted input.
        // Do not recompute amount_in from the rounded next price.
        let next = get_next_sqrt_price_from_input(
            sqrt_ratio_current_x96,
            liquidity,
            amount_remaining_less_fee,
            zero_for_one,
        )?;

        (next, amount_remaining_less_fee)
    };

    let amount_out = match (zero_for_one, reached_target) {
        (true, true) => get_amount1_delta_with_liquidity_delta(liquidity_delta_sqrt, false),
        (true, false) => {
            let (amount_out, _) = get_amount1_delta(
                sqrt_ratio_next_x96,
                sqrt_ratio_current_x96,
                liquidity.unwrap(),
                false,
            )?;
            amount_out
        }
        (false, true) => get_amount0_delta_with_liquidity_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity_delta_sqrt,
            false,
        )?,
        (false, false) => {
            let (amount_out, _) = get_amount0_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_next_x96,
                liquidity.unwrap(),
                false,
            )?;
            amount_out
        }
    };

    let fee_amount = if reached_target {
        fee_on_exact_input(amount_in, fee)?
    } else {
        // Safety: amount_in == amount_remaining_less_fee <= amount_remaining.
        amount_remaining - amount_in
    };

    Ok(SwapStep {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

#[inline]
fn compute_exact_out(
    sqrt_ratio_current_x96: SqrtPriceX96,
    sqrt_ratio_target_x96: SqrtPriceX96,
    liquidity: NonZeroLiquidity,
    amount_remaining: U256,
    fee: Fee,
    zero_for_one: bool,
) -> Result<SwapStep, SwapMathError> {
    debug_assert!(!fee.is_max());

    let (max_amount_out, liquidity_delta_sqrt) = if zero_for_one {
        get_amount1_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity.unwrap(),
            false,
        )?
    } else {
        get_amount0_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity.unwrap(),
            false,
        )?
    };

    let (sqrt_ratio_next_x96, amount_out) = if amount_remaining >= max_amount_out {
        (sqrt_ratio_target_x96, max_amount_out)
    } else {
        let next = get_next_sqrt_price_from_output(
            sqrt_ratio_current_x96,
            liquidity,
            amount_remaining,
            zero_for_one,
        )?;

        (next, amount_remaining)
    };

    let reached_target = amount_remaining >= max_amount_out;

    let amount_in = match (zero_for_one, reached_target) {
        (true, true) => get_amount0_delta_with_liquidity_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity_delta_sqrt,
            true,
        )?,
        (true, false) => {
            let (amount_in, _) = get_amount0_delta(
                sqrt_ratio_next_x96,
                sqrt_ratio_current_x96,
                liquidity.unwrap(),
                true,
            )?;
            amount_in
        }
        (false, true) => get_amount1_delta_with_liquidity_delta(liquidity_delta_sqrt, true),
        (false, false) => {
            let (amount_in, _) = get_amount1_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_next_x96,
                liquidity.unwrap(),
                true,
            )?;
            amount_in
        }
    };

    let fee_amount = fee_on_exact_input(amount_in, fee)?;

    Ok(SwapStep {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Sign;
    use proptest::prelude::*;
    use ruint::aliases::{U160, U256};

    fn u256(s: &str) -> U256 {
        U256::from_str_radix(s, 10).unwrap()
    }

    fn ether(n: u128) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    fn sqrt_price_unchecked(value: U256) -> SqrtPriceX96 {
        unsafe { SqrtPriceX96::new_unchecked(value.to::<U160>()) }
    }

    fn sqrt_price(s: &str) -> SqrtPriceX96 {
        sqrt_price_unchecked(u256(s))
    }

    fn liq(n: u128) -> NonZeroLiquidity {
        NonZeroLiquidity::new(n).unwrap()
    }

    fn fee(pips: u32) -> Fee {
        Fee::new(pips).unwrap()
    }

    fn exact_in(amount: U256) -> I256 {
        I256::checked_from_sign_and_abs(Sign::Negative, amount).unwrap()
    }

    fn exact_out(amount: U256) -> I256 {
        I256::checked_from_sign_and_abs(Sign::Positive, amount).unwrap()
    }

    /// √(1/1) · 2^96
    fn sqrt_price_1_1() -> SqrtPriceX96 {
        sqrt_price_unchecked(U256::ONE << 96)
    }

    /// √(101/100) · 2^96
    fn sqrt_price_101_100() -> SqrtPriceX96 {
        sqrt_price("79623317895830914510639640423")
    }

    /// √(1000/100) · 2^96 — well above sqrt_price_1_1; used as upper target
    fn sqrt_price_1000_100() -> SqrtPriceX96 {
        sqrt_price("250541448375047925191586663628")
    }

    /// √(10000/100) · 2^96 — far above sqrt_price_1_1; used as upper target
    fn sqrt_price_10000_100() -> SqrtPriceX96 {
        sqrt_price("792281625142643375935439503360")
    }

    /// √(1/4) · 2^96
    fn sqrt_price_1_4() -> U256 {
        u256("39614081257132168796771975168")
    }

    /// Non-zero U160 in the valid range for Uniswap sqrt prices.
    fn arb_sqrt_price() -> impl Strategy<Value = SqrtPriceX96> {
        any::<[u64; 3]>()
            .prop_map(|limbs| {
                let top = limbs[2] & 0xFFFF_FFFF;
                U256::from_limbs([limbs[0], limbs[1], top, 0])
            })
            .prop_filter_map("price must be in protocol range", SqrtPriceX96::from_u256)
    }

    fn arb_u256() -> impl Strategy<Value = U256> {
        any::<[u64; 4]>().prop_map(U256::from_limbs)
    }

    fn arb_i256_magnitude() -> impl Strategy<Value = U256> {
        arb_u256().prop_filter("amount must fit int256", |amount| {
            I256::checked_from_sign_and_abs(Sign::Positive, *amount).is_some()
        })
    }

    proptest! {
        #[test]
        fn fuzz_get_sqrt_price_target(
            zero_for_one in any::<bool>(),
            next  in arb_sqrt_price(),
            limit in arb_sqrt_price(),
        ) {
            let result = get_sqrt_price_target(zero_for_one, next, limit);
            let expected = if zero_for_one { next.max(limit) } else { next.min(limit) };
            prop_assert_eq!(result, expected);
        }
    }

    #[test]
    fn fee_too_large_is_rejected_by_fee_type() {
        assert_eq!(Fee::new(MAX_FEE + 1), None);
    }

    #[test]
    fn max_fee_exact_out_returns_error() {
        let err = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(1_000_000),
            exact_out(ether(1)),
            fee(MAX_FEE),
        );
        assert_eq!(err, Err(SwapMathError::MaxFeeExactOut));
    }

    #[test]
    fn max_fee_exact_in_consumes_all_as_fee_without_price_movement() {
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(1_000_000),
            exact_in(ether(1)),
            fee(MAX_FEE),
        )
        .unwrap();

        assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_1_1());
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, ether(1));
    }

    #[test]
    fn exact_in_one_for_zero_capped_at_price_target() {
        // Current price below target: one_for_zero (price moves up).
        // Large input; step is bounded by the tick target.
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(2_000_000_000_000_000_000),
            exact_in(ether(1)),
            fee(600),
        )
        .unwrap();

        assert_eq!(step.amount_in, u256("9975124224178055"));
        assert_eq!(step.amount_out, u256("9925619580021728"));
        assert_eq!(step.fee_amount, u256("5988667735148"));
        // Total spend must be strictly less than the full input.
        assert!(step.amount_in + step.fee_amount < ether(1));
        assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_101_100());
    }

    #[test]
    fn exact_out_one_for_zero_capped_at_price_target() {
        // Large desired output; step is bounded by the tick target.
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(2_000_000_000_000_000_000),
            exact_out(ether(1)),
            fee(600),
        )
        .unwrap();

        assert_eq!(step.amount_in, u256("9975124224178055"));
        assert_eq!(step.amount_out, u256("9925619580021728"));
        assert_eq!(step.fee_amount, u256("5988667735148"));
        assert!(step.amount_out < ether(1));
        assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_101_100());
    }

    #[test]
    fn exact_in_one_for_zero_fully_spent() {
        // Target price is far away; the entire input is consumed without crossing.
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_1000_100(),
            liq(2_000_000_000_000_000_000),
            exact_in(ether(1)),
            fee(600),
        )
        .unwrap();

        assert_eq!(step.amount_in, u256("999400000000000000"));
        assert_eq!(step.amount_out, u256("666399946655997866"));
        assert_eq!(step.fee_amount, u256("600000000000000"));
        // Net input + fee == full input amount.
        assert_eq!(step.amount_in + step.fee_amount, ether(1));
        assert!(step.sqrt_ratio_next_x96 < sqrt_price_1000_100());
    }

    #[test]
    fn exact_out_one_for_zero_fully_received() {
        // Target is far; the desired output is fully deliverable.
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_10000_100(),
            liq(2_000_000_000_000_000_000),
            exact_out(ether(1)),
            fee(600),
        )
        .unwrap();

        assert_eq!(step.amount_in, u256("2000000000000000000"));
        assert_eq!(step.fee_amount, u256("1200720432259356"));
        assert_eq!(step.amount_out, ether(1));
        assert!(step.sqrt_ratio_next_x96 < sqrt_price_10000_100());
    }

    #[test]
    fn amount_out_capped_at_desired() {
        let step = compute_swap_step(
            sqrt_price("417332158212080721273783715441582"),
            sqrt_price_unchecked(u256("1452870262520218020823638996")),
            liq(159344665391607089467575320103u128),
            exact_out(U256::ONE),
            fee(1),
        )
        .unwrap();

        assert_eq!(step.amount_in, U256::ONE);
        assert_eq!(step.fee_amount, U256::ONE);
        assert_eq!(step.amount_out, U256::ONE);
        assert_eq!(
            step.sqrt_ratio_next_x96.as_u256(),
            u256("417332158212080721273783715441581")
        );
    }

    #[test]
    fn target_price_one_partial_input() {
        // An extremely small target price of 1.
        let amount = u256("3915081100057732413702495386755767");
        let step = compute_swap_step(
            sqrt_price_unchecked(U256::from(2u64)),
            sqrt_price_unchecked(U256::ONE),
            liq(1),
            exact_in(amount),
            fee(1),
        )
        .unwrap();

        assert_eq!(step.amount_in, sqrt_price_1_4());
        assert_eq!(step.fee_amount, u256("39614120871253040049813"));
        assert!(step.amount_in + step.fee_amount <= amount);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.sqrt_ratio_next_x96.as_u256(), U256::ONE);
    }

    #[test]
    fn not_entire_input_taken_as_fee() {
        let current = sqrt_price_1_1();
        let step = compute_swap_step(
            current,
            sqrt_price_101_100(),
            liq(1985041575832132834610021537970u128),
            exact_in(U256::from(10u64)),
            fee(1872),
        )
        .unwrap();

        assert_eq!(step.amount_in, U256::from(9u64));
        assert_eq!(step.fee_amount, U256::ONE);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.sqrt_ratio_next_x96, current);
    }

    #[test]
    fn zero_for_one_insufficient_liquidity_exact_out() {
        // zero_for_one: current > target  →  price moves DOWN.
        let sqrt_p_raw = u256("20282409603651670423947251286016");
        let sqrt_p = sqrt_price_unchecked(sqrt_p_raw);
        // Target is 10 % below current; liq=1024 can only deliver ≈ 26,214 token1
        // across this range, so requesting 100_000 triggers the "step hits target" path.
        let sqrt_p_target = sqrt_price_unchecked(sqrt_p_raw * U256::from(9u64) / U256::from(10u64));

        let step = compute_swap_step(
            sqrt_p,
            sqrt_p_target,
            liq(1024),
            exact_out(U256::from(100_000u64)), // > max_available ≈ 26,214
            fee(3000),
        )
        .unwrap();

        // Pool is exhausted before delivering the full request; price hits the boundary.
        assert_eq!(step.sqrt_ratio_next_x96, sqrt_p_target);
        assert!(step.amount_out <= U256::from(100_000u64));
        assert!(step.amount_out > U256::ZERO);
    }

    #[test]
    fn one_for_zero_insufficient_liquidity_exact_out() {
        // one_for_zero: current < target (price moves UP).
        let sqrt_p_raw = u256("20282409603651670423947251286016");
        let sqrt_p = sqrt_price_unchecked(sqrt_p_raw);
        // The target is above the current price for one_for_zero.
        let sqrt_p_target =
            sqrt_price_unchecked(sqrt_p_raw * U256::from(11u64) / U256::from(10u64));

        let step = compute_swap_step(
            sqrt_p,
            sqrt_p_target,
            liq(1024),
            exact_out(U256::from(263000u64)),
            fee(3000),
        )
        .unwrap();

        // With tiny liquidity the step reaches the target boundary.
        assert_eq!(step.sqrt_ratio_next_x96, sqrt_p_target);
        assert!(step.amount_out <= U256::from(263000u64));
    }

    proptest! {
        /// For any valid exact-input swap step:
        /// 1. `amount_in + fee_amount` does not overflow U256.
        /// 2. The total spend never exceeds `amount_remaining`.
        /// 3. The resulting price stays between `current` and `target` (inclusive).
        /// 4. If current == target, nothing moves (trivial case).
        /// 5. If price did NOT reach the target, `amount_in + fee == amount_remaining`.
        #[test]
        fn fuzz_exact_in_invariants(
            sqrt_price        in arb_sqrt_price(),
            sqrt_price_target in arb_sqrt_price(),
            liquidity         in 1u128..=u128::MAX,
            amount_remaining  in arb_i256_magnitude(),
            fee_pips          in 0u32..=MAX_FEE,
        ) {
            let result = compute_swap_step(
                sqrt_price,
                sqrt_price_target,
                NonZeroLiquidity::new(liquidity).unwrap(),
                exact_in(amount_remaining),
                fee(fee_pips),
            );

            // Skip arithmetic errors (tested separately).
            let step = match result {
                Ok(s) => s,
                Err(_) => return Ok(()),
            };

            // 1. No overflow in total spend.
            prop_assert!(
                step.amount_in <= U256::MAX - step.fee_amount,
                "amount_in + fee_amount overflows U256"
            );

            // 2. Total spend ≤ remaining input.
            prop_assert!(
                step.amount_in + step.fee_amount <= amount_remaining,
                "spent more than amount_remaining"
            );

            // 3. Price stays within [current, target] (or [target, current]).
            if sqrt_price_target <= sqrt_price {
                prop_assert!(step.sqrt_ratio_next_x96 <= sqrt_price);
                prop_assert!(step.sqrt_ratio_next_x96 >= sqrt_price_target);
            } else {
                prop_assert!(step.sqrt_ratio_next_x96 >= sqrt_price);
                prop_assert!(step.sqrt_ratio_next_x96 <= sqrt_price_target);
            }

            // 4. Trivial case: current == target → nothing moves.
            if sqrt_price == sqrt_price_target {
                prop_assert_eq!(step.amount_in,  U256::ZERO);
                prop_assert_eq!(step.amount_out, U256::ZERO);
                prop_assert_eq!(step.fee_amount, U256::ZERO);
                prop_assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_target);
            }

            // 5. If price did NOT reach target, all input is fully consumed.
            if step.sqrt_ratio_next_x96 != sqrt_price_target {
                prop_assert_eq!(
                    step.amount_in + step.fee_amount,
                    amount_remaining,
                    "unfilled step did not consume full amount_remaining"
                );
            }
        }

        /// For any valid exact-output swap step:
        /// 1. `amount_in + fee_amount` does not overflow U256.
        /// 2. `amount_out ≤ amount_remaining`.
        /// 3. The resulting price stays within [current, target].
        /// 4. If price did NOT reach target, `amount_out == amount_remaining`.
        #[test]
        fn fuzz_exact_out_invariants(
            sqrt_price        in arb_sqrt_price(),
            sqrt_price_target in arb_sqrt_price(),
            liquidity         in 1u128..=u128::MAX,
            amount_remaining  in arb_i256_magnitude(),
            fee_pips          in 0u32..MAX_FEE, // strict: MAX_SWAP_FEE excluded for exact-out
        ) {
            let result = compute_swap_step(
                sqrt_price,
                sqrt_price_target,
                NonZeroLiquidity::new(liquidity).unwrap(),
                exact_out(amount_remaining),
                fee(fee_pips),
            );

            let step = match result {
                Ok(s) => s,
                Err(_) => return Ok(()),
            };

            // 1. No overflow in input + fee.
            prop_assert!(
                step.amount_in <= U256::MAX - step.fee_amount,
                "amount_in + fee_amount overflows U256"
            );

            // 2. Output never exceeds the requested amount.
            prop_assert!(step.amount_out <= amount_remaining, "produced more output than requested");

            // 3. Price stays within bounds.
            if sqrt_price_target <= sqrt_price {
                prop_assert!(step.sqrt_ratio_next_x96 <= sqrt_price);
                prop_assert!(step.sqrt_ratio_next_x96 >= sqrt_price_target);
            } else {
                prop_assert!(step.sqrt_ratio_next_x96 >= sqrt_price);
                prop_assert!(step.sqrt_ratio_next_x96 <= sqrt_price_target);
            }

            // 4. Trivial case.
            if sqrt_price == sqrt_price_target {
                prop_assert_eq!(step.amount_in,  U256::ZERO);
                prop_assert_eq!(step.amount_out, U256::ZERO);
                prop_assert_eq!(step.fee_amount, U256::ZERO);
                prop_assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_target);
            }

            // 5. Partial fill → exact output was delivered.
            if step.sqrt_ratio_next_x96 != sqrt_price_target {
                prop_assert_eq!(
                    step.amount_out,
                    amount_remaining,
                    "partial fill did not deliver full requested output"
                );
            }
        }
    }
}
