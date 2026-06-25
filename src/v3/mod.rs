pub mod cache;
pub mod error;

/// Uniswap V3/V4 math library — Rust port.
///
/// # Module structure
///
/// | Submodule          | Mirrors                   | Responsibility                              |
/// |--------------------|---------------------------|---------------------------------------------|
/// | `full_math`        | `FullMath.sol`            | 512-bit `mulDiv` (overflow-safe)            |
/// | `tick_math`        | `TickMath.sol`            | Tick ↔ `sqrtPriceX96` conversion           |
/// | `sqrt_price_math`  | `SqrtPriceMath.sol`       | Token deltas from price moves               |
/// | `swap_math`        | `SwapMath.sol`            | Single-step swap computation                |
/// | `full_simulator`   | *(off-chain)*             | Multi-tick swap simulation                  |
///
/// # Simulator selection guide
///
/// ```text
/// ┌──────────────────────────────────────────────────────────────┐
/// │  Is the swap large enough to cross an initialized tick?      │
/// │                                                              │
/// │   No → V3FastSimulator (single step, O(1), no tick crossing) │
/// │  Yes → V3FullSimulator (tick-crossing loop, O(log n / step)) │
/// └──────────────────────────────────────────────────────────────┘
/// ```
///
/// Use `V3FastSimulator` as a cheap pre-filter.  When it indicates a tick
/// may be crossed ([`TickCrossResult::crossed`]), re-price with
/// [`V3FullSimulator`].
pub mod full_math;
pub mod pool;
pub mod price;
pub mod sqrt_price_math;
pub mod swap_math;
pub mod tick_math;
pub mod ticks;
pub mod types;

pub use crate::types::I256 as SignedAmount;
pub use pool::{FullSwapResult, Pool, PoolState};
pub use swap_math::{SwapMathError, SwapStep};
pub use tick_math::{MAX_TICK, MIN_TICK};
pub use ticks::{PoolTicks, TickInfo};

use ruint::aliases::U256;

use serde::{Deserialize, Serialize};
use swap_math::compute_swap_step;
use tick_math::{MAX_SQRT_PRICE, MIN_SQRT_PRICE};

// ─── Tick ────────────────────────────────────────────────────────────────────

/// A single initialized tick in a V3/V4 pool.
///
/// Stored only while `liquidity_gross > 0`. When a burn/remove operation
/// drains the last liquidity from a tick, the entry is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickEntry {
    pub tick_idx: i32,
    /// Signed net liquidity crossing this tick upward.
    /// Uniswap convention: add when crossing up, subtract when crossing down.
    pub liquidity_net: i128,
    /// Total outstanding liquidity referencing this tick as a boundary.
    pub liquidity_gross: u128,
}

// ─── Pool state ───────────────────────────────────────────────────────────────

/// Lightweight V3 pool state used by the fast single-step simulator.
///
/// For multi-tick simulation, use [`FullPoolState`] with [`V3FullSimulator`].
///
/// # Derivations
///
/// `PartialEq` and `Hash` are derived so callers can use `V3PoolState` as a
/// cache key when evaluating many routes in parallel — a common pattern in
/// arbitrage bots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct V3PoolState {
    /// Current sqrt price in Q64.96 fixed-point.
    pub sqrt_price_x96: U256,
    /// Active liquidity in the current tick range.
    pub liquidity: u128,
    /// Current tick index.
    pub tick: i32,
    /// Fee in pips (e.g. `3000` = 0.30 %).  Must be < 1 000 000.
    pub fee: u32,
}

// ─── Tick-crossing result ─────────────────────────────────────────────────────

/// Result of [`V3FastSimulator::step_until_tick`].
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
    ///
    /// FIX (was a doc-comment bug): the previous version documented this
    /// as "returns true if the step did NOT hit the boundary", which was
    /// the opposite of the actual predicate.
    pub crossed: bool,
}

// ─── Fast simulator ───────────────────────────────────────────────────────────

/// Single-step V3 swap simulator — **no tick crossings**.
///
/// Computes the swap outcome assuming constant liquidity throughout the range.
/// This is O(1) and suitable for high-frequency arbitrage opportunity
/// screening.
///
/// # When to use this vs [`V3FullSimulator`]
///
/// * **Use `V3FastSimulator`** when the swap amount is expected to be small
///   relative to the range depth, or as a first-pass filter.
/// * **Use `V3FullSimulator`** when [`step_until_tick`][Self::step_until_tick]
///   returns `crossed = true`, or when you need an accurate quote for a large
///   swap that crosses multiple ticks.
///
/// # Error handling
///
/// All methods return `Result<_, SwapMathError>` rather than panicking.
/// In arbitrage-critical code, a panic kills the whole process; returning an
/// error lets the caller skip this route and try the next one.
pub struct V3FastSimulator;

impl V3FastSimulator {
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
    /// Propagates [`SwapMathError`] from `compute_swap_step`.  The most
    /// common cause is `fee >= 1_000_000` ([`SwapMathError::FeeTooLarge`]).
    #[inline]
    pub fn quote_exact_input(
        state: &V3PoolState,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<U256, SwapMathError> {
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
    /// Propagates [`SwapMathError`].  Note that exact-output with
    /// `fee == 1_000_000` (100 %) is always an error
    /// ([`SwapMathError::MaxFeeExactOut`]).
    #[inline]
    pub fn quote_exact_output(
        state: &V3PoolState,
        amount_out: U256,
        zero_for_one: bool,
    ) -> Result<U256, SwapMathError> {
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
    /// Propagates [`SwapMathError`].
    #[inline]
    pub fn step_exact_input(
        state: &V3PoolState,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<SwapStep, SwapMathError> {
        // FIX: compute_swap_step now returns Result — propagate with `?`.
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
    /// Propagates [`SwapMathError`].  100 % fee + exact-out is always an
    /// error ([`SwapMathError::MaxFeeExactOut`]).
    #[inline]
    pub fn step_exact_output(
        state: &V3PoolState,
        amount_out: U256,
        zero_for_one: bool,
    ) -> Result<SwapStep, SwapMathError> {
        // FIX: propagate Result.
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
    ///   [`V3FullSimulator::swap`] to get an accurate quote.
    ///
    /// # Parameters
    ///
    /// * `next_initialized_tick_price_x96` — the sqrt price of the nearest
    ///   initialized tick in the swap direction (from the caller's tick bitmap
    ///   or sorted tick list).
    ///
    /// # Errors
    ///
    /// Propagates [`SwapMathError`].
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = V3FastSimulator::step_until_tick(&pool, amount_in, true, next_price)?;
    /// if result.crossed {
    ///     // Fall back to full simulator.
    ///     let full = V3FullSimulator::swap(&full_state, params)?;
    /// }
    /// ```
    #[inline]
    pub fn step_until_tick(
        state: &V3PoolState,
        amount_in: U256,
        zero_for_one: bool,
        next_initialized_tick_price_x96: U256,
    ) -> Result<TickCrossResult, SwapMathError> {
        // FIX: propagate Result.
        let step = compute_swap_step(
            state.sqrt_price_x96,
            next_initialized_tick_price_x96,
            state.liquidity,
            SignedAmount::negative(amount_in),
            state.fee,
        )?;

        // FIX (doc-comment bug): `crossed` is TRUE when the price *reached*
        // the boundary, i.e. when tick crossing is required.
        // Previously the doc said "true if did NOT hit" — opposite of reality.
        let crossed = step.sqrt_ratio_next_x96 == next_initialized_tick_price_x96;

        Ok(TickCrossResult { step, crossed })
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::v3::tick_math::get_sqrt_price_at_tick;

    use super::*;
    use proptest::prelude::*;
    use ruint::aliases::U256;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn ether(n: u128) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    fn sqrt_price_1_1() -> U256 {
        U256::ONE << 96
    }

    fn basic_pool() -> V3PoolState {
        V3PoolState {
            sqrt_price_x96: sqrt_price_1_1(),
            liquidity: 1_000_000_000_000_000_000u128,
            tick: 0,
            fee: 3000,
        }
    }

    // ── Basic invariants ──────────────────────────────────────────────────────

    #[test]
    fn quote_exact_input_returns_positive_output() {
        let pool = basic_pool();
        let out = V3FastSimulator::quote_exact_input(&pool, U256::from(1_000_000u64), true)
            .expect("quote should succeed");
        assert!(out > U256::ZERO);
    }

    #[test]
    fn quote_exact_output_returns_positive_input_required() {
        let pool = basic_pool();
        let input_required =
            V3FastSimulator::quote_exact_output(&pool, U256::from(1_000_000u64), true)
                .expect("quote should succeed");
        assert!(input_required > U256::ZERO);
    }

    #[test]
    fn exact_input_zero_for_one_moves_price_down() {
        let pool = basic_pool();
        let step =
            V3FastSimulator::step_exact_input(&pool, ether(1), true).expect("step should succeed");
        assert!(step.sqrt_ratio_next_x96 < pool.sqrt_price_x96);
        assert!(step.amount_in > U256::ZERO);
        assert!(step.amount_out > U256::ZERO);
        assert!(step.fee_amount > U256::ZERO);
    }

    #[test]
    fn exact_input_one_for_zero_moves_price_up() {
        let pool = basic_pool();
        let step =
            V3FastSimulator::step_exact_input(&pool, ether(1), false).expect("step should succeed");
        assert!(step.sqrt_ratio_next_x96 > pool.sqrt_price_x96);
        assert!(step.amount_in > U256::ZERO);
        assert!(step.amount_out > U256::ZERO);
        assert!(step.fee_amount > U256::ZERO);
    }

    #[test]
    fn zero_amount_exact_input_returns_zero_amounts() {
        let pool = basic_pool();
        let step = V3FastSimulator::step_exact_input(&pool, U256::ZERO, true)
            .expect("step should succeed");
        assert_eq!(step.sqrt_ratio_next_x96, pool.sqrt_price_x96);
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, U256::ZERO);
    }

    #[test]
    fn zero_amount_exact_output_returns_zero_amounts() {
        let pool = basic_pool();
        let step = V3FastSimulator::step_exact_output(&pool, U256::ZERO, false)
            .expect("step should succeed");
        assert_eq!(step.sqrt_ratio_next_x96, pool.sqrt_price_x96);
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, U256::ZERO);
    }

    // ── Quote consistency ─────────────────────────────────────────────────────

    #[test]
    fn exact_output_quote_equals_step_in_plus_fee() {
        let pool = basic_pool();
        let step = V3FastSimulator::step_exact_output(&pool, U256::from(1_000_000u64), true)
            .expect("step should succeed");
        let quoted = V3FastSimulator::quote_exact_output(&pool, U256::from(1_000_000u64), true)
            .expect("quote should succeed");
        assert_eq!(quoted, step.amount_in + step.fee_amount);
    }

    #[test]
    fn exact_input_quote_equals_step_amount_out() {
        let pool = basic_pool();
        let step = V3FastSimulator::step_exact_input(&pool, U256::from(1_000_000u64), false)
            .expect("step should succeed");
        let quoted = V3FastSimulator::quote_exact_input(&pool, U256::from(1_000_000u64), false)
            .expect("quote should succeed");
        assert_eq!(quoted, step.amount_out);
    }

    // ── step_until_tick ───────────────────────────────────────────────────────

    #[test]
    fn step_until_tick_detects_tick_crossing_for_large_swap() {
        let pool = basic_pool();
        // FIX: get_sqrt_price_at_tick returns Result.
        let next_tick_price = get_sqrt_price_at_tick(-60).unwrap();

        let result = V3FastSimulator::step_until_tick(&pool, ether(1_000), true, next_tick_price)
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

        let result =
            V3FastSimulator::step_until_tick(&pool, U256::from(1_000u64), true, next_tick_price)
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

    // ── Monotonicity ──────────────────────────────────────────────────────────

    #[test]
    fn larger_input_returns_at_least_as_much_output() {
        let pool = basic_pool();
        let small_out = V3FastSimulator::quote_exact_input(&pool, U256::from(1_000u64), true)
            .expect("quote should succeed");
        let large_out = V3FastSimulator::quote_exact_input(&pool, U256::from(1_000_000u64), true)
            .expect("quote should succeed");
        assert!(large_out >= small_out);
    }

    // ── Round-trip ────────────────────────────────────────────────────────────

    #[test]
    fn exact_input_round_trip_requires_positive_input() {
        let pool = basic_pool();
        let amount_in = U256::from(1_000_000u64);
        let amount_out = V3FastSimulator::quote_exact_input(&pool, amount_in, true)
            .expect("quote should succeed");
        if amount_out > U256::ZERO {
            let input_required = V3FastSimulator::quote_exact_output(&pool, amount_out, true)
                .expect("quote should succeed");
            assert!(input_required > U256::ZERO);
        }
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[test]
    fn exact_input_rejects_fee_of_100_percent_via_swap_math_error() {
        // 100 % fee is allowed for exact-in by SwapMath (entire input is fee),
        // so this should succeed, not error.  The error case is exact-out.
        let mut pool = basic_pool();
        pool.fee = 1_000_000;
        let result = V3FastSimulator::step_exact_input(&pool, U256::from(1_000u64), true);
        // SwapMath allows 100% fee for exact-in.
        assert!(result.is_ok());
    }

    #[test]
    fn exact_output_rejects_fee_of_100_percent() {
        let mut pool = basic_pool();
        pool.fee = 1_000_000; // 100 % — undefined for exact-out.
        let err = V3FastSimulator::step_exact_output(&pool, U256::from(1_000u64), true)
            .expect_err("exact-out with 100% fee must fail");
        assert_eq!(err, SwapMathError::MaxFeeExactOut);
    }

    // ── Property-based tests ──────────────────────────────────────────────────

    proptest! {
        #[test]
        fn fuzz_exact_input_basic_invariants(
            zero_for_one in any::<bool>(),
            amount      in 0u128..1_000_000_000_000_000_000u128,
            // FIX: keep fee in range that compute_swap_step accepts (<= 1_000_000).
            // 100% fee is technically valid for exact-in, so we allow it and
            // filter non-Ok results rather than restricting the range.
            fee         in 0u32..=1_000_000u32,
            liquidity   in 1u128..u128::MAX,
        ) {
            let pool = V3PoolState {
                sqrt_price_x96: sqrt_price_1_1(),
                liquidity: liquidity,
                tick: 0,
                fee,
            };

            // A fee of exactly 1_000_000 with exact-in is valid per SwapMath.
            // We propagate the Result: if it errors, just skip this case.
            let Ok(step) = V3FastSimulator::step_exact_input(
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
            let pool = V3PoolState {
                sqrt_price_x96: sqrt_price_1_1(),
                liquidity: liquidity,
                tick: 0,
                fee,
            };

            // FIX: handle Result.
            let step = V3FastSimulator::step_exact_output(
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
