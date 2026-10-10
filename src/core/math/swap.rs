//! Optimized port of Uniswap `SwapMath`: single-step swap computation within
//! one initialized-tick range.
//!
//! Design goals:
//! - Keep Uniswap-compatible rounding and fee semantics.
//! - No tracing or generic FullMath in the hot arithmetic path: fee math uses
//!   exact `U256 * u32 / u32` long division with the fee complement cached on
//!   [`Fee`].
//! - Avoid cloning `SqrtPriceX96` (not `Copy`). Prices are passed as
//!   [`Cow`]: a step that reaches its target hands the caller's target price
//!   back unchanged (`Cow::Borrowed` stays borrowed), and only a partial fill
//!   allocates a new `Cow::Owned` price.
//! - Exact-input partial fills consume the entire remaining input: whatever is
//!   not swapped is charged as fee, exactly as in Uniswap.

use std::borrow::Cow;

use alloy::primitives::I256;
use ruint::aliases::U256;

use crate::core::types::fee::{Fee, MAX_FEE};
use crate::core::types::nonzero::{NonZeroLiquidity, NonZeroU256};
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

/// Result of a single swap step.
///
/// `'a` is the lifetime of the prices passed to [`compute_swap_step`]: when
/// the step ends on the current or target price, that input `Cow` is returned
/// as-is instead of being cloned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapStep<'a> {
    /// Sqrt price after the step. Equal to the target price when the step
    /// reached it; otherwise strictly between current and target.
    pub sqrt_ratio_next_x96: Cow<'a, SqrtPriceX96>,
    /// Input token amount swapped into the pool, excluding `fee_amount`.
    pub amount_in: U256,
    /// Output token amount taken out of the pool.
    pub amount_out: U256,
    /// Fee charged on the input token, on top of `amount_in`.
    pub fee_amount: U256,
}

/// Returns the exact-input amount left after deducting the swap fee.
///
/// Formula:
///
/// ```text
/// floor(amount_remaining * (MAX_FEE - fee_pips) / MAX_FEE)
/// ```
///
/// Rounds down so the pool never swaps more than the caller paid for.
#[inline(always)]
pub(crate) fn amount_less_fee_exact_in(
    amount_remaining: U256,
    fee: &Fee,
) -> Result<U256, MathError> {
    if fee.is_zero() {
        return Ok(amount_remaining);
    }

    if fee.is_max() {
        return Ok(U256::ZERO);
    }

    mul_div_u32_floor(amount_remaining, fee.complement(), MAX_FEE)
}

/// Returns the fee owed on top of a net swap input `amount_in`.
///
/// Formula:
///
/// ```text
/// ceil(amount_in * fee_pips / (MAX_FEE - fee_pips))
/// ```
///
/// Rounds up in favor of the pool. With a 100% fee the whole `amount_in` is
/// returned, matching Uniswap's `MAX_SWAP_FEE` special case.
#[inline(always)]
pub(crate) fn fee_on_exact_input(amount_in: U256, fee: &Fee) -> Result<U256, MathError> {
    if amount_in.is_zero() || fee.is_zero() {
        return Ok(U256::ZERO);
    }

    if fee.is_max() {
        return Ok(amount_in);
    }

    mul_div_u32_ceil(amount_in, fee.pips(), fee.complement())
}

/// Clamps the next tick price by the caller's price limit.
///
/// For `zero_for_one` swaps price moves down, so the target is the larger of
/// the two; otherwise it is the smaller. Returns a reference to whichever
/// input wins, so callers can pass it on as `Cow::Borrowed` without cloning.
#[inline(always)]
pub fn get_sqrt_price_target<'a>(
    zero_for_one: bool,
    sqrt_price_next: &'a SqrtPriceX96,
    sqrt_price_limit: &'a SqrtPriceX96,
) -> &'a SqrtPriceX96 {
    if zero_for_one {
        sqrt_price_next.max(sqrt_price_limit)
    } else {
        sqrt_price_next.min(sqrt_price_limit)
    }
}

/// Computes one swap step from `sqrt_price_current` towards
/// `sqrt_price_target` within a single liquidity range.
///
/// `amount` follows the Uniswap V4 sign convention:
/// - negative: exact input, `|amount|` of the input token remains to be spent.
/// - positive: exact output, `|amount|` of the output token remains to be
///   received.
///
/// The swap direction is implied by the prices: `current >= target` means
/// `zero_for_one` (price moves down).
///
/// A zero `amount` is a no-op that returns the current price. It is checked
/// before the fee guard, so it never errors.
///
/// # Errors
///
/// - [`SwapMathError::MaxFeeExactOut`] for an exact-output step with a 100%
///   fee: no finite input can pay the fee and still buy output.
/// - Sqrt-price math errors (price overflow, insufficient reserves) from the
///   underlying amount and next-price computations.
#[inline]
#[must_use = "discarding a swap step result silently drops errors"]
pub fn compute_swap_step<'a>(
    sqrt_price_current: &'a SqrtPriceX96,
    sqrt_price_target: &'a SqrtPriceX96,
    liquidity: &'_ NonZeroLiquidity,
    amount: &'_ I256,
    fee: &'_ Fee,
) -> Result<SwapStep<'a>, SwapMathError> {
    if amount.is_zero() {
        return Ok(SwapStep {
            sqrt_ratio_next_x96: Cow::Borrowed(sqrt_price_current),
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            fee_amount: U256::ZERO,
        });
    }

    let exact_in = amount.is_negative();

    if !exact_in && fee.is_max() {
        return Err(SwapMathError::MaxFeeExactOut);
    }

    let zero_for_one = sqrt_price_current >= sqrt_price_target;
    // Safety: the zero amount was returned above, and `unsigned_abs` of a
    // non-zero `I256` (including `I256::MIN`) is non-zero.
    let amount_remaining = unsafe { NonZeroU256::new_unchecked(amount.unsigned_abs()) };

    if exact_in {
        compute_exact_in(
            Cow::Borrowed(sqrt_price_current),
            Cow::Borrowed(sqrt_price_target),
            liquidity,
            amount_remaining,
            fee,
            zero_for_one,
        )
    } else {
        compute_exact_out(
            Cow::Borrowed(sqrt_price_current),
            Cow::Borrowed(sqrt_price_target),
            liquidity,
            amount_remaining,
            fee,
            zero_for_one,
        )
    }
}

/// Exact-input step.
///
/// The fee is deducted up front. If the remaining net input covers the whole
/// range, the step ends on the target and the fee is computed on the input
/// actually used. Otherwise the step ends inside the range, the full net input
/// is swapped, and everything the caller supplied beyond it is the fee.
#[inline]
fn compute_exact_in<'a>(
    sqrt_price_current: Cow<'a, SqrtPriceX96>,
    sqrt_price_target: Cow<'a, SqrtPriceX96>,
    liquidity: &NonZeroLiquidity,
    amount_remaining: NonZeroU256,
    fee: &Fee,
    zero_for_one: bool,
) -> Result<SwapStep<'a>, SwapMathError> {
    let amount_remaining_less_fee = amount_less_fee_exact_in(*amount_remaining, fee)?;

    // The fee absorbs the whole input (100% fee, or a dust amount that floors
    // to zero). Uniswap still charges all of it as fee without moving the
    // price, unless `current == target`: then the range is empty, the step is
    // treated as reaching the target with `amount_in = 0`, and no fee is due.
    let Some(amount_remaining_less_fee) = NonZeroU256::new(amount_remaining_less_fee) else {
        let fee_amount = if sqrt_price_current == sqrt_price_target {
            U256::ZERO
        } else {
            amount_remaining.unwrap()
        };

        return Ok(SwapStep {
            sqrt_ratio_next_x96: sqrt_price_current,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            fee_amount,
        });
    };

    let (max_amount_in, liquidity_delta_sqrt) = if zero_for_one {
        get_amount0_delta(
            &sqrt_price_target,
            &sqrt_price_current,
            liquidity.unwrap(),
            true,
        )?
    } else {
        get_amount1_delta(
            &sqrt_price_current,
            &sqrt_price_target,
            liquidity.unwrap(),
            true,
        )?
    };

    let reached_target = *amount_remaining_less_fee >= max_amount_in;

    let (amount_out, amount_in, sqrt_ratio_next_x96) = if reached_target {
        (
            if zero_for_one {
                get_amount1_delta_with_liquidity_delta(liquidity_delta_sqrt, false)
            } else {
                get_amount0_delta_with_liquidity_delta(
                    &sqrt_price_current,
                    &sqrt_price_target,
                    liquidity_delta_sqrt,
                    false,
                )?
            },
            max_amount_in,
            sqrt_price_target,
        )
    } else {
        let sqrt_ratio_next_x96 = get_next_sqrt_price_from_input(
            &sqrt_price_current,
            liquidity,
            &amount_remaining_less_fee,
            zero_for_one,
        )?;

        (
            if zero_for_one {
                get_amount1_delta(
                    &sqrt_ratio_next_x96,
                    &sqrt_price_current,
                    liquidity.unwrap(),
                    false,
                )?
                .0
            } else {
                get_amount0_delta(
                    &sqrt_price_current,
                    &sqrt_ratio_next_x96,
                    liquidity.unwrap(),
                    false,
                )?
                .0
            },
            // Uniswap-compatible accounting: a partial fill consumes the full
            // fee-adjusted input. Do not recompute it from the rounded price.
            amount_remaining_less_fee.unwrap(),
            Cow::Owned(sqrt_ratio_next_x96),
        )
    };

    let fee_amount = if reached_target {
        fee_on_exact_input(amount_in, fee)?
    } else {
        // Safety: amount_in == amount_remaining_less_fee <= amount_remaining.
        *amount_remaining - amount_in
    };

    Ok(SwapStep {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

/// Exact-output step.
///
/// Output is capped at what the range can deliver. The required input is
/// rounded up, and the fee is always charged on top of that input.
///
/// Callers must reject a 100% fee before calling this.
#[inline]
fn compute_exact_out<'a>(
    sqrt_price_current: Cow<'a, SqrtPriceX96>,
    sqrt_price_target: Cow<'a, SqrtPriceX96>,
    liquidity: &'_ NonZeroLiquidity,
    amount_remaining: NonZeroU256,
    fee: &'_ Fee,
    zero_for_one: bool,
) -> Result<SwapStep<'a>, SwapMathError> {
    debug_assert!(!fee.is_max());

    let (max_amount_out, liquidity_delta_sqrt) = if zero_for_one {
        get_amount1_delta(
            &sqrt_price_target,
            &sqrt_price_current,
            liquidity.unwrap(),
            false,
        )?
    } else {
        get_amount0_delta(
            &sqrt_price_current,
            &sqrt_price_target,
            liquidity.unwrap(),
            false,
        )?
    };

    let (amount_in, amount_out, sqrt_ratio_next_x96) = if *amount_remaining >= max_amount_out {
        (
            if zero_for_one {
                get_amount0_delta_with_liquidity_delta(
                    &sqrt_price_target,
                    &sqrt_price_current,
                    liquidity_delta_sqrt,
                    true,
                )?
            } else {
                get_amount1_delta_with_liquidity_delta(liquidity_delta_sqrt, true)
            },
            max_amount_out,
            sqrt_price_target,
        )
    } else {
        let sqrt_ratio_next_x96 = get_next_sqrt_price_from_output(
            &sqrt_price_current,
            liquidity,
            &amount_remaining,
            zero_for_one,
        )?;

        (
            if zero_for_one {
                get_amount0_delta(
                    &sqrt_ratio_next_x96,
                    &sqrt_price_current,
                    liquidity.unwrap(),
                    true,
                )?
                .0
            } else {
                get_amount1_delta(
                    &sqrt_price_current,
                    &sqrt_ratio_next_x96,
                    liquidity.unwrap(),
                    true,
                )?
                .0
            },
            amount_remaining.unwrap(),
            Cow::Owned(sqrt_ratio_next_x96),
        )
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

    /// Owned-argument wrapper around [`compute_swap_step`] for fixed-value
    /// cases that do not care about borrowing.
    fn swap_step(
        current: SqrtPriceX96,
        target: SqrtPriceX96,
        liquidity: NonZeroLiquidity,
        amount: I256,
        fee: Fee,
    ) -> Result<SwapStep<'static>, SwapMathError> {
        compute_swap_step(&current, &target, &liquidity, &amount, &fee).map(|step| SwapStep {
            sqrt_ratio_next_x96: Cow::Owned(step.sqrt_ratio_next_x96.into_owned()),
            amount_in: step.amount_in,
            amount_out: step.amount_out,
            fee_amount: step.fee_amount,
        })
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
            let result = get_sqrt_price_target(zero_for_one, &next, &limit);
            let expected = if zero_for_one { (&next).max(&limit) } else { (&next).min(&limit) };
            prop_assert_eq!(result, expected);
        }
    }

    #[test]
    fn amount_less_fee_exact_in_edge_cases() {
        let amount = u256("123456789");

        assert_eq!(amount_less_fee_exact_in(amount, &fee(0)), Ok(amount));
        assert_eq!(
            amount_less_fee_exact_in(amount, &fee(MAX_FEE)),
            Ok(U256::ZERO)
        );
        // floor(10 * 998_128 / 1_000_000) = floor(9.98) = 9
        assert_eq!(
            amount_less_fee_exact_in(U256::from(10u64), &fee(1872)),
            Ok(U256::from(9u64))
        );
    }

    #[test]
    fn fee_on_exact_input_edge_cases() {
        let amount = u256("123456789");

        assert_eq!(fee_on_exact_input(U256::ZERO, &fee(3000)), Ok(U256::ZERO));
        assert_eq!(fee_on_exact_input(amount, &fee(0)), Ok(U256::ZERO));
        assert_eq!(fee_on_exact_input(amount, &fee(MAX_FEE)), Ok(amount));
        // ceil(9 * 1872 / 998_128) = ceil(0.0168..) = 1
        assert_eq!(
            fee_on_exact_input(U256::from(9u64), &fee(1872)),
            Ok(U256::ONE)
        );
    }

    #[test]
    fn fee_too_large_is_rejected_by_fee_type() {
        assert_eq!(Fee::new(MAX_FEE + 1), None);
    }

    #[test]
    fn zero_amount_is_noop_even_with_max_fee_exact_out() {
        let current = sqrt_price_1_1();
        let target = sqrt_price_101_100();

        let step = compute_swap_step(
            &current,
            &target,
            &liq(1_000_000),
            &I256::ZERO,
            &fee(MAX_FEE),
        )
        .unwrap();

        assert!(matches!(
            step.sqrt_ratio_next_x96,
            Cow::Borrowed(price) if std::ptr::eq(price, &current)
        ));
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, U256::ZERO);
    }

    #[test]
    fn max_fee_exact_out_returns_error() {
        let err = swap_step(
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
        let step = swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(1_000_000),
            exact_in(ether(1)),
            fee(MAX_FEE),
        )
        .unwrap();

        assert_eq!(*step.sqrt_ratio_next_x96, sqrt_price_1_1());
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, ether(1));
    }

    #[test]
    fn exact_in_dust_amount_is_charged_entirely_as_fee() {
        // floor(1 * 997_000 / 1_000_000) = 0: nothing is left to swap, but the
        // input is still consumed so the pool swap loop makes progress.
        let step = swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(1_000_000),
            exact_in(U256::ONE),
            fee(3000),
        )
        .unwrap();

        assert_eq!(*step.sqrt_ratio_next_x96, sqrt_price_1_1());
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, U256::ONE);
    }

    #[test]
    fn exact_in_fee_absorbs_all_at_target_is_noop() {
        // current == target: the range is empty, so no fee is charged even
        // though the fee would absorb the whole input.
        let step = swap_step(
            sqrt_price_1_1(),
            sqrt_price_1_1(),
            liq(1_000_000),
            exact_in(ether(1)),
            fee(MAX_FEE),
        )
        .unwrap();

        assert_eq!(*step.sqrt_ratio_next_x96, sqrt_price_1_1());
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, U256::ZERO);
    }

    #[test]
    fn reached_target_borrows_target_price() {
        let current = sqrt_price_1_1();
        let target = sqrt_price_101_100();

        for amount in [exact_in(ether(1)), exact_out(ether(1))] {
            let step = compute_swap_step(
                &current,
                &target,
                &liq(2_000_000_000_000_000_000),
                &amount,
                &fee(600),
            )
            .unwrap();

            assert!(matches!(
                step.sqrt_ratio_next_x96,
                Cow::Borrowed(price) if std::ptr::eq(price, &target)
            ));
        }
    }

    #[test]
    fn partial_fill_returns_owned_price() {
        let current = sqrt_price_1_1();
        let target = sqrt_price_10000_100();

        for amount in [exact_in(ether(1)), exact_out(ether(1))] {
            let step = compute_swap_step(
                &current,
                &target,
                &liq(2_000_000_000_000_000_000),
                &amount,
                &fee(600),
            )
            .unwrap();

            assert!(matches!(step.sqrt_ratio_next_x96, Cow::Owned(_)));
            assert!(*step.sqrt_ratio_next_x96 > current);
            assert!(*step.sqrt_ratio_next_x96 < target);
        }
    }

    #[test]
    fn exact_in_zero_for_one_capped_at_price_target() {
        // Current price above target: zero_for_one (price moves down).
        let step = swap_step(
            sqrt_price_101_100(),
            sqrt_price_1_1(),
            liq(2_000_000_000_000_000_000),
            exact_in(ether(1)),
            fee(600),
        )
        .unwrap();

        assert_eq!(*step.sqrt_ratio_next_x96, sqrt_price_1_1());
        assert!(step.amount_in > U256::ZERO);
        assert!(step.amount_out > U256::ZERO);
        assert_eq!(
            step.fee_amount,
            fee_on_exact_input(step.amount_in, &fee(600)).unwrap()
        );
        assert!(step.amount_in + step.fee_amount < ether(1));
    }

    #[test]
    fn exact_in_one_for_zero_capped_at_price_target() {
        // Current price below target: one_for_zero (price moves up).
        // Large input; step is bounded by the tick target.
        let step = swap_step(
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
        assert_eq!(*step.sqrt_ratio_next_x96, sqrt_price_101_100());
    }

    #[test]
    fn exact_out_one_for_zero_capped_at_price_target() {
        // Large desired output; step is bounded by the tick target.
        let step = swap_step(
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
        assert_eq!(*step.sqrt_ratio_next_x96, sqrt_price_101_100());
    }

    #[test]
    fn exact_in_one_for_zero_fully_spent() {
        // Target price is far away; the entire input is consumed without crossing.
        let step = swap_step(
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
        assert!(*step.sqrt_ratio_next_x96 < sqrt_price_1000_100());
    }

    #[test]
    fn exact_out_one_for_zero_fully_received() {
        // Target is far; the desired output is fully deliverable.
        let step = swap_step(
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
        assert!(*step.sqrt_ratio_next_x96 < sqrt_price_10000_100());
    }

    #[test]
    fn amount_out_capped_at_desired() {
        let step = swap_step(
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
        let step = swap_step(
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
        let step = swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(1985041575832132834610021537970u128),
            exact_in(U256::from(10u64)),
            fee(1872),
        )
        .unwrap();

        assert_eq!(step.amount_in, U256::from(9u64));
        assert_eq!(step.fee_amount, U256::ONE);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(*step.sqrt_ratio_next_x96, sqrt_price_1_1());
    }

    #[test]
    fn zero_for_one_insufficient_liquidity_exact_out() {
        // zero_for_one: current > target  →  price moves DOWN.
        let sqrt_p_raw = u256("20282409603651670423947251286016");
        let sqrt_p = sqrt_price_unchecked(sqrt_p_raw);
        // Target is 10 % below current; liq=1024 can only deliver ≈ 26,214 token1
        // across this range, so requesting 100_000 triggers the "step hits target" path.
        let sqrt_p_target = sqrt_price_unchecked(sqrt_p_raw * U256::from(9u64) / U256::from(10u64));

        let step = swap_step(
            sqrt_p,
            sqrt_p_target.clone(),
            liq(1024),
            exact_out(U256::from(100_000u64)), // > max_available ≈ 26,214
            fee(3000),
        )
        .unwrap();

        // Pool is exhausted before delivering the full request; price hits the boundary.
        assert_eq!(*step.sqrt_ratio_next_x96, sqrt_p_target);
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

        let step = swap_step(
            sqrt_p,
            sqrt_p_target.clone(),
            liq(1024),
            exact_out(U256::from(263000u64)),
            fee(3000),
        )
        .unwrap();

        // With tiny liquidity the step reaches the target boundary.
        assert_eq!(*step.sqrt_ratio_next_x96, sqrt_p_target);
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
                &sqrt_price,
                &sqrt_price_target,
                &NonZeroLiquidity::new(liquidity).unwrap(),
                &exact_in(amount_remaining),
                &fee(fee_pips),
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
                prop_assert!(*step.sqrt_ratio_next_x96 <= sqrt_price);
                prop_assert!(*step.sqrt_ratio_next_x96 >= sqrt_price_target);
            } else {
                prop_assert!(*step.sqrt_ratio_next_x96 >= sqrt_price);
                prop_assert!(*step.sqrt_ratio_next_x96 <= sqrt_price_target);
            }

            // 4. Trivial case: current == target → nothing moves.
            if sqrt_price == sqrt_price_target {
                prop_assert_eq!(step.amount_in,  U256::ZERO);
                prop_assert_eq!(step.amount_out, U256::ZERO);
                prop_assert_eq!(step.fee_amount, U256::ZERO);
                prop_assert_eq!(&*step.sqrt_ratio_next_x96, &sqrt_price_target);
            }

            // 5. If price did NOT reach target, all input is fully consumed.
            if *step.sqrt_ratio_next_x96 != sqrt_price_target {
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
        /// 4. If current == target, nothing moves (trivial case).
        /// 5. If price did NOT reach target, `amount_out == amount_remaining`.
        #[test]
        fn fuzz_exact_out_invariants(
            sqrt_price        in arb_sqrt_price(),
            sqrt_price_target in arb_sqrt_price(),
            liquidity         in 1u128..=u128::MAX,
            amount_remaining  in arb_i256_magnitude(),
            fee_pips          in 0u32..MAX_FEE, // strict: MAX_FEE is rejected for exact-out
        ) {
            let result = compute_swap_step(
                &sqrt_price,
                &sqrt_price_target,
                &NonZeroLiquidity::new(liquidity).unwrap(),
                &exact_out(amount_remaining),
                &fee(fee_pips),
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
                prop_assert!(*step.sqrt_ratio_next_x96 <= sqrt_price);
                prop_assert!(*step.sqrt_ratio_next_x96 >= sqrt_price_target);
            } else {
                prop_assert!(*step.sqrt_ratio_next_x96 >= sqrt_price);
                prop_assert!(*step.sqrt_ratio_next_x96 <= sqrt_price_target);
            }

            // 4. Trivial case.
            if sqrt_price == sqrt_price_target {
                prop_assert_eq!(step.amount_in,  U256::ZERO);
                prop_assert_eq!(step.amount_out, U256::ZERO);
                prop_assert_eq!(step.fee_amount, U256::ZERO);
                prop_assert_eq!(&*step.sqrt_ratio_next_x96, &sqrt_price_target);
            }

            // 5. Partial fill → exact output was delivered.
            if *step.sqrt_ratio_next_x96 != sqrt_price_target {
                prop_assert_eq!(
                    step.amount_out,
                    amount_remaining,
                    "partial fill did not deliver full requested output"
                );
            }
        }
    }
}
