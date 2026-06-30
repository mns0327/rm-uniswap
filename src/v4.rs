/// Uniswap V4-compatible facade.
///
/// The public API mirrors the Solidity library names while using Rust method
/// names: [`FullMath`], [`SqrtPriceMath`], [`SwapMath`], and [`TickMath`].
///
/// Use [`Quoter`] for single-range quotes that cannot cross an initialized
/// tick. Use [`Pool`] when a swap may cross ticks or when committed pool state
/// must be updated.
pub use crate::Error;
pub use crate::core::concentrated::pool::{
    FullSwapResult, ModifyLiquidityParams, ModifyLiquidityResult, Pool, PoolSnapshot, PoolState,
    SwapParams, SwapSimulationResult, TickCrossInfo, TickCrossing, delta_amount_in,
    delta_amount_out, tick_spacing_to_max_liquidity_per_tick,
};
pub use crate::core::concentrated::ticks::PoolTicks;
pub use crate::core::math::swap::SwapStep;
pub use crate::core::types::delta::BalanceDelta;
pub use crate::core::types::signed::I256 as SignedAmount;
pub use crate::core::types::{PoolTicksSnapshot, TickInfo};

use ruint::aliases::U256;

use crate::core::math::{
    full as full_math, sqrt_price as sqrt_price_math, swap as swap_math, tick as tick_math,
};
use swap_math::compute_swap_step;
use tick_math::{MAX_SQRT_PRICE, MIN_SQRT_PRICE};

#[cfg(feature = "positions")]
pub mod positions {
    pub use crate::core::concentrated::position_manager::*;
}

/// Solidity-compatible facade for `FullMath.sol`.
pub struct FullMath;

impl FullMath {
    pub fn mul_div(a: U256, b: U256, denominator: U256) -> Result<U256, Error> {
        full_math::mul_div(a, b, denominator)
    }

    pub fn mul_div_rounding_up(a: U256, b: U256, denominator: U256) -> Result<U256, Error> {
        full_math::mul_div_rounding_up(a, b, denominator)
    }

    pub fn mul_shift_right(a: U256, b: U256, shift: u32) -> Result<U256, Error> {
        full_math::mul_shift_right(a, b, shift)
    }

    pub fn mul_shift_right_rounding_up(a: U256, b: U256, shift: u32) -> Result<U256, Error> {
        full_math::mul_shift_right_rounding_up(a, b, shift)
    }
}

/// Solidity-compatible facade for `SqrtPriceMath.sol`.
pub struct SqrtPriceMath;

impl SqrtPriceMath {
    pub fn get_next_sqrt_price_from_amount0_rounding_up(
        sqrt_price_x96: U256,
        liquidity: U256,
        amount: U256,
        add: bool,
    ) -> Result<U256, Error> {
        sqrt_price_math::get_next_sqrt_price_from_amount0_rounding_up(
            sqrt_price_x96,
            liquidity,
            amount,
            add,
        )
    }

    pub fn get_next_sqrt_price_from_amount1_rounding_down(
        sqrt_price_x96: U256,
        liquidity: U256,
        amount: U256,
        add: bool,
    ) -> Result<U256, Error> {
        sqrt_price_math::get_next_sqrt_price_from_amount1_rounding_down(
            sqrt_price_x96,
            liquidity,
            amount,
            add,
        )
    }

    pub fn get_amount0_delta(
        sqrt_a_x96: U256,
        sqrt_b_x96: U256,
        liquidity: U256,
        round_up: bool,
    ) -> Result<U256, Error> {
        sqrt_price_math::get_amount0_delta(sqrt_a_x96, sqrt_b_x96, liquidity, round_up)
    }

    pub fn get_amount1_delta(
        sqrt_a_x96: U256,
        sqrt_b_x96: U256,
        liquidity: U256,
        round_up: bool,
    ) -> Result<U256, Error> {
        sqrt_price_math::get_amount1_delta(sqrt_a_x96, sqrt_b_x96, liquidity, round_up)
    }

    pub fn get_next_sqrt_price_from_input(
        sqrt_price_x96: U256,
        liquidity: U256,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<U256, Error> {
        sqrt_price_math::get_next_sqrt_price_from_input(
            sqrt_price_x96,
            liquidity,
            amount_in,
            zero_for_one,
        )
    }

    pub fn get_next_sqrt_price_from_output(
        sqrt_price_x96: U256,
        liquidity: U256,
        amount_out: U256,
        zero_for_one: bool,
    ) -> Result<U256, Error> {
        sqrt_price_math::get_next_sqrt_price_from_output(
            sqrt_price_x96,
            liquidity,
            amount_out,
            zero_for_one,
        )
    }
}

/// Solidity-compatible facade for `SwapMath.sol`.
pub struct SwapMath;

impl SwapMath {
    pub const MAX_SWAP_FEE: u32 = swap_math::MAX_SWAP_FEE;

    pub fn compute_swap_step(
        sqrt_price_current_x96: U256,
        sqrt_price_target_x96: U256,
        liquidity: u128,
        amount_remaining: SignedAmount,
        fee_pips: u32,
    ) -> Result<SwapStep, Error> {
        swap_math::compute_swap_step(
            sqrt_price_current_x96,
            sqrt_price_target_x96,
            liquidity,
            amount_remaining,
            fee_pips,
        )
    }

    pub fn get_sqrt_price_target(
        zero_for_one: bool,
        sqrt_price_next_x96: U256,
        sqrt_price_limit_x96: U256,
    ) -> U256 {
        swap_math::get_sqrt_price_target(zero_for_one, sqrt_price_next_x96, sqrt_price_limit_x96)
    }
}

/// Solidity-compatible facade for `TickMath.sol`.
pub struct TickMath;

impl TickMath {
    pub const MIN_TICK: i32 = tick_math::MIN_TICK;
    pub const MAX_TICK: i32 = tick_math::MAX_TICK;
    pub const MIN_SQRT_PRICE: U256 = tick_math::MIN_SQRT_PRICE;
    pub const MAX_SQRT_PRICE: U256 = tick_math::MAX_SQRT_PRICE;

    pub fn get_sqrt_price_at_tick(tick: i32) -> Result<U256, Error> {
        tick_math::get_sqrt_price_at_tick(tick)
    }

    pub fn get_tick_at_sqrt_price(sqrt_price_x96: U256) -> Result<i32, Error> {
        tick_math::get_tick_at_sqrt_price(sqrt_price_x96)
    }

    pub fn max_usable_tick(tick_spacing: i32) -> Result<i32, Error> {
        tick_math::max_usable_tick(tick_spacing)
    }

    pub fn min_usable_tick(tick_spacing: i32) -> Result<i32, Error> {
        tick_math::min_usable_tick(tick_spacing)
    }

    pub fn most_significant_bit(value: U256) -> Result<u32, Error> {
        tick_math::most_significant_bit(value)
    }
}

/// Lightweight V4 pool state used by the fast single-step simulator.
///
/// For multi-tick simulation, use [`PoolState`] with [`Pool`].
///
/// # Derivations
///
/// `PartialEq` and `Hash` are derived so callers can use `QuoteState` as a
/// cache key when evaluating many routes in parallel — a common pattern in
/// arbitrage bots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QuoteState {
    /// Current sqrt price in Q64.96 fixed-point.
    pub sqrt_price_x96: U256,
    /// Active liquidity in the current tick range.
    pub liquidity: u128,
    /// Current tick index.
    pub tick: i32,
    /// Fee in pips (e.g. `3000` = 0.30 %).  Must be < 1 000 000.
    pub fee: u32,
}

/// Result of [`Quoter::step_until_tick`].
///
/// Using a named struct instead of a raw tuple makes the `crossed` field
/// self-documenting at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickCrossResult {
    /// Outcome of the swap step up to the tick boundary.
    pub step: SwapStep,
    /// `true` if the price reached `next_initialized_tick_price_x96`,
    /// meaning the swap consumed all liquidity in this range and a tick
    /// crossing would occur.
    pub crossed: bool,
}

/// Single-step V4 swap simulator — **no tick crossings**.
///
/// Computes the swap outcome assuming constant liquidity throughout the range.
/// This is O(1) and suitable for high-frequency arbitrage opportunity
/// screening.
///
/// # When to use this vs [`Pool`]
///
/// * **Use `Quoter`** when the swap amount is expected to be small
///   relative to the range depth, or as a first-pass filter.
/// * **Use `Pool`** when [`step_until_tick`][Self::step_until_tick]
///   returns `crossed = true`, or when you need an accurate quote for a large
///   swap that crosses multiple ticks.
///
/// # Error handling
///
/// All methods return `Result<_, Error>` rather than panicking.
/// In arbitrage-critical code, a panic kills the whole process; returning an
/// error lets the caller skip this route and try the next one.
pub struct Quoter;

impl Quoter {
    /// Quote the output for an exact-input swap.
    ///
    /// Runs a single swap step with the price limit set to the extreme of the
    /// valid range so the step is never cut short by the limit.
    ///
    /// # Returns
    ///
    /// `Ok(amount_out)` — the amount of the output token the caller receives.
    ///
    /// # Errors
    ///
    /// Propagates [`Error`] from `compute_swap_step`. The most common cause is
    /// `fee >= 1_000_000` ([`Error::FeeTooLarge`]).
    #[inline]
    pub fn quote_exact_input(
        state: &QuoteState,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<U256, Error> {
        Ok(Self::step_exact_input(state, amount_in, zero_for_one)?.amount_out)
    }

    /// Quote the input required for an exact-output swap.
    ///
    /// # Returns
    ///
    /// `Ok(amount_in_total)` — `amount_in + fee_amount` from the pool's
    /// perspective (what the caller must provide).
    ///
    /// # Errors
    ///
    /// Propagates [`Error`]. Note that exact-output with
    /// `fee == 1_000_000` (100 %) is always an error
    /// ([`Error::MaxFeeExactOut`]).
    #[inline]
    pub fn quote_exact_output(
        state: &QuoteState,
        amount_out: U256,
        zero_for_one: bool,
    ) -> Result<U256, Error> {
        let step = Self::step_exact_output(state, amount_out, zero_for_one)?;
        Ok(step.amount_in + step.fee_amount)
    }

    /// Compute the full swap step details for an exact-input swap.
    ///
    /// Returns the complete [`SwapStep`] including `amount_in`, `amount_out`,
    /// `fee_amount`, and `sqrt_ratio_next_x96`.
    ///
    /// Use this when you need the resulting price, not just the output amount.
    ///
    /// # Errors
    ///
    /// Propagates [`Error`].
    #[inline]
    pub fn step_exact_input(
        state: &QuoteState,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<SwapStep, Error> {
        compute_swap_step(
            state.sqrt_price_x96,
            // Use the extreme price limit so this single step is never
            // cut short; the caller can use `step_until_tick` to bound it.
            extreme_price_target(zero_for_one),
            state.liquidity,
            SignedAmount::negative(amount_in),
            state.fee,
        )
    }

    /// Compute the full swap step details for an exact-output swap.
    ///
    /// # Errors
    ///
    /// Propagates [`Error`]. A 100% fee with exact-output is always an error
    /// ([`Error::MaxFeeExactOut`]).
    #[inline]
    pub fn step_exact_output(
        state: &QuoteState,
        amount_out: U256,
        zero_for_one: bool,
    ) -> Result<SwapStep, Error> {
        compute_swap_step(
            state.sqrt_price_x96,
            extreme_price_target(zero_for_one),
            state.liquidity,
            SignedAmount::positive(amount_out),
            state.fee,
        )
    }

    /// Run an exact-input swap step bounded by `next_initialized_tick_price_x96`.
    ///
    /// This is the primary tool for deciding whether a full tick-crossing
    /// simulation is necessary:
    ///
    /// * If [`TickCrossResult::crossed`] is `false`, the swap stayed within
    ///   the current tick range — the fast result is accurate.
    /// * If [`TickCrossResult::crossed`] is `true`, the price reached the
    ///   next initialized tick boundary and you **must** call
    ///   [`Pool::swap`] to get an accurate quote.
    ///
    /// # Parameters
    ///
    /// * `next_initialized_tick_price_x96` — the sqrt price of the nearest
    ///   initialized tick in the swap direction (from the caller's tick bitmap
    ///   or sorted tick list).
    ///
    /// # Errors
    ///
    /// Propagates [`Error`].
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = Quoter::step_until_tick(&pool, amount_in, next_price)?;
    /// if result.crossed {
    ///     // Fall back to full simulator.
    ///     let full = Pool::swap(&full_state, params)?;
    /// }
    /// ```
    #[inline]
    pub fn step_until_tick(
        state: &QuoteState,
        amount_in: U256,
        next_initialized_tick_price_x96: U256,
    ) -> Result<TickCrossResult, Error> {
        let step = compute_swap_step(
            state.sqrt_price_x96,
            next_initialized_tick_price_x96,
            state.liquidity,
            SignedAmount::negative(amount_in),
            state.fee,
        )?;

        let crossed = step.sqrt_ratio_next_x96 == next_initialized_tick_price_x96;

        Ok(TickCrossResult { step, crossed })
    }
}

/// Return the most extreme valid price target in the given swap direction.
///
/// We offset by ±1 from the absolute minimum / maximum because
/// `min_sqrt_price()` and `max_sqrt_price()` are the *invalid* sentinels in
/// `tick_math`; the valid range is `(min, max]`.  Using `min + 1` / `max - 1`
/// keeps the price target inside the valid range so sqrt-price math never
/// encounters a boundary overflow.
#[inline]
fn extreme_price_target(zero_for_one: bool) -> U256 {
    if zero_for_one {
        MIN_SQRT_PRICE + U256::ONE
    } else {
        MAX_SQRT_PRICE - U256::ONE
    }
}

#[cfg(test)]
mod tests {
    use crate::core::math::tick::get_sqrt_price_at_tick;

    use super::*;
    use proptest::prelude::*;
    use ruint::aliases::U256;

    fn ether(n: u128) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    fn sqrt_price_1_1() -> U256 {
        U256::ONE << 96
    }

    fn basic_pool() -> QuoteState {
        QuoteState {
            sqrt_price_x96: sqrt_price_1_1(),
            liquidity: 1_000_000_000_000_000_000u128,
            tick: 0,
            fee: 3000,
        }
    }

    #[test]
    fn quote_exact_input_returns_positive_output() {
        let pool = basic_pool();
        let out = Quoter::quote_exact_input(&pool, U256::from(1_000_000u64), true)
            .expect("quote should succeed");
        assert!(out > U256::ZERO);
    }

    #[test]
    fn quote_exact_output_returns_positive_input_required() {
        let pool = basic_pool();
        let input_required = Quoter::quote_exact_output(&pool, U256::from(1_000_000u64), true)
            .expect("quote should succeed");
        assert!(input_required > U256::ZERO);
    }

    #[test]
    fn exact_input_zero_for_one_moves_price_down() {
        let pool = basic_pool();
        let step = Quoter::step_exact_input(&pool, ether(1), true).expect("step should succeed");
        assert!(step.sqrt_ratio_next_x96 < pool.sqrt_price_x96);
        assert!(step.amount_in > U256::ZERO);
        assert!(step.amount_out > U256::ZERO);
        assert!(step.fee_amount > U256::ZERO);
    }

    #[test]
    fn exact_input_one_for_zero_moves_price_up() {
        let pool = basic_pool();
        let step = Quoter::step_exact_input(&pool, ether(1), false).expect("step should succeed");
        assert!(step.sqrt_ratio_next_x96 > pool.sqrt_price_x96);
        assert!(step.amount_in > U256::ZERO);
        assert!(step.amount_out > U256::ZERO);
        assert!(step.fee_amount > U256::ZERO);
    }

    #[test]
    fn zero_amount_exact_input_returns_zero_amounts() {
        let pool = basic_pool();
        let step = Quoter::step_exact_input(&pool, U256::ZERO, true).expect("step should succeed");
        assert_eq!(step.sqrt_ratio_next_x96, pool.sqrt_price_x96);
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, U256::ZERO);
    }

    #[test]
    fn zero_amount_exact_output_returns_zero_amounts() {
        let pool = basic_pool();
        let step =
            Quoter::step_exact_output(&pool, U256::ZERO, false).expect("step should succeed");
        assert_eq!(step.sqrt_ratio_next_x96, pool.sqrt_price_x96);
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, U256::ZERO);
    }

    #[test]
    fn exact_output_quote_equals_step_in_plus_fee() {
        let pool = basic_pool();
        let step = Quoter::step_exact_output(&pool, U256::from(1_000_000u64), true)
            .expect("step should succeed");
        let quoted = Quoter::quote_exact_output(&pool, U256::from(1_000_000u64), true)
            .expect("quote should succeed");
        assert_eq!(quoted, step.amount_in + step.fee_amount);
    }

    #[test]
    fn exact_input_quote_equals_step_amount_out() {
        let pool = basic_pool();
        let step = Quoter::step_exact_input(&pool, U256::from(1_000_000u64), false)
            .expect("step should succeed");
        let quoted = Quoter::quote_exact_input(&pool, U256::from(1_000_000u64), false)
            .expect("quote should succeed");
        assert_eq!(quoted, step.amount_out);
    }

    #[test]
    fn step_until_tick_detects_tick_crossing_for_large_swap() {
        let pool = basic_pool();
        let next_tick_price = get_sqrt_price_at_tick(-60).unwrap();

        let result = Quoter::step_until_tick(&pool, ether(1_000), next_tick_price)
            .expect("step should succeed");

        // A large swap should reach the tick boundary.
        assert!(
            result.crossed,
            "expected tick to be crossed for a large swap"
        );
    }

    #[test]
    fn step_until_tick_detects_no_crossing_for_small_swap() {
        let pool = basic_pool();
        let next_tick_price = get_sqrt_price_at_tick(-60).unwrap();

        let result = Quoter::step_until_tick(&pool, U256::from(1_000u64), next_tick_price)
            .expect("step should succeed");

        // A tiny swap should not reach the tick boundary.
        assert!(
            !result.crossed,
            "expected no tick crossing for a small swap"
        );
        // Price should have moved down but not reached the tick.
        assert!(result.step.sqrt_ratio_next_x96 > next_tick_price);
        assert!(result.step.sqrt_ratio_next_x96 < pool.sqrt_price_x96);
    }

    #[test]
    fn larger_input_returns_at_least_as_much_output() {
        let pool = basic_pool();
        let small_out = Quoter::quote_exact_input(&pool, U256::from(1_000u64), true)
            .expect("quote should succeed");
        let large_out = Quoter::quote_exact_input(&pool, U256::from(1_000_000u64), true)
            .expect("quote should succeed");
        assert!(large_out >= small_out);
    }

    #[test]
    fn exact_input_round_trip_requires_positive_input() {
        let pool = basic_pool();
        let amount_in = U256::from(1_000_000u64);
        let amount_out =
            Quoter::quote_exact_input(&pool, amount_in, true).expect("quote should succeed");
        if amount_out > U256::ZERO {
            let input_required =
                Quoter::quote_exact_output(&pool, amount_out, true).expect("quote should succeed");
            assert!(input_required > U256::ZERO);
        }
    }

    #[test]
    fn exact_input_rejects_fee_of_100_percent_via_swap_math_error() {
        // 100 % fee is allowed for exact-in by SwapMath (entire input is fee),
        // so this should succeed, not error.  The error case is exact-out.
        let mut pool = basic_pool();
        pool.fee = 1_000_000;
        let result = Quoter::step_exact_input(&pool, U256::from(1_000u64), true);
        // SwapMath allows 100% fee for exact-in.
        assert!(result.is_ok());
    }

    #[test]
    fn exact_output_rejects_fee_of_100_percent() {
        let mut pool = basic_pool();
        pool.fee = 1_000_000; // 100 % — undefined for exact-out.
        let err = Quoter::step_exact_output(&pool, U256::from(1_000u64), true)
            .expect_err("exact-out with 100% fee must fail");
        assert_eq!(err, Error::MaxFeeExactOut);
    }

    proptest! {
        #[test]
        fn fuzz_exact_input_basic_invariants(
            zero_for_one in any::<bool>(),
            amount      in 0u128..1_000_000_000_000_000_000u128,
            // 100% fee is technically valid for exact-in, so we allow it and
            // filter non-Ok results rather than restricting the range.
            fee         in 0u32..=1_000_000u32,
            liquidity   in 1u128..u128::MAX,
        ) {
            let pool = QuoteState {
                sqrt_price_x96: sqrt_price_1_1(),
                liquidity,
                tick: 0,
                fee,
            };

            // A fee of exactly 1_000_000 with exact-in is valid per SwapMath.
            // We propagate the Result: if it errors, just skip this case.
            let Ok(step) = Quoter::step_exact_input(
                &pool,
                U256::from(amount),
                zero_for_one,
            ) else {
                return Ok(());
            };

            prop_assert!(step.amount_in <= U256::from(amount));
            prop_assert!(step.amount_in + step.fee_amount <= U256::from(amount));

            if amount == 0 {
                prop_assert_eq!(step.sqrt_ratio_next_x96, pool.sqrt_price_x96);
                prop_assert_eq!(step.amount_in, U256::ZERO);
                prop_assert_eq!(step.amount_out, U256::ZERO);
                prop_assert_eq!(step.fee_amount, U256::ZERO);
            } else if zero_for_one {
                prop_assert!(step.sqrt_ratio_next_x96 <= pool.sqrt_price_x96);
            } else {
                prop_assert!(step.sqrt_ratio_next_x96 >= pool.sqrt_price_x96);
            }
        }

        #[test]
        fn fuzz_exact_output_basic_invariants(
            zero_for_one in any::<bool>(),
            amount_out  in 0u128..1_000_000_000_000_000u128,
            // Exclude 100% fee: exact-out with MAX_SWAP_FEE is always an error.
            fee         in 0u32..999_999u32,
            liquidity   in 1u128..u128::MAX,
        ) {
            let pool = QuoteState {
                sqrt_price_x96: sqrt_price_1_1(),
                liquidity,
                tick: 0,
                fee,
            };

            let step = Quoter::step_exact_output(
                &pool,
                U256::from(amount_out),
                zero_for_one,
            )
            .expect("exact-out step should succeed for fee < 1_000_000");

            prop_assert!(step.amount_out <= U256::from(amount_out));
            prop_assert!(step.amount_in <= U256::MAX - step.fee_amount);

            if amount_out == 0 {
                prop_assert_eq!(step.sqrt_ratio_next_x96, pool.sqrt_price_x96);
                prop_assert_eq!(step.amount_in, U256::ZERO);
                prop_assert_eq!(step.amount_out, U256::ZERO);
                prop_assert_eq!(step.fee_amount, U256::ZERO);
            } else if zero_for_one {
                prop_assert!(step.sqrt_ratio_next_x96 <= pool.sqrt_price_x96);
            } else {
                prop_assert!(step.sqrt_ratio_next_x96 >= pool.sqrt_price_x96);
            }
        }
    }
}
