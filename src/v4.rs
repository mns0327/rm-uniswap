//! Uniswap V4 math — thin adapter over V3 fast simulation primitives.
//!
//! V4 uses the same concentrated-liquidity swap math as V3.
//! For single-step quotes without tick crossing, `tick_spacing` is carried
//! for PoolKey completeness but does not affect the computation.

use ruint::aliases::U256;

pub use crate::v3::{BalanceDelta, FullMath, SqrtPriceMath, SwapMath, SwapStep, TickMath};
use crate::{
    Error,
    v3::{QuoteState, Quoter},
};

/// Uniswap V4 pool state.
///
/// Mirrors the V3 quote state but includes explicit `tick_spacing`,
/// because V4 stores tick spacing directly in the PoolKey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    /// Current sqrt price X96.
    pub sqrt_price_x96: U256,

    /// Active liquidity at the current tick range.
    pub liquidity: u128,

    /// Current tick.
    pub tick: i32,

    /// Fee in pips, e.g. 3000 = 0.3%.
    pub fee: u32,

    /// Explicit V4 PoolKey tick spacing.
    pub tick_spacing: i32,
}

impl PoolState {
    /// Convert this V4 state into a V3-compatible state.
    ///
    /// `tick_spacing` is intentionally dropped because it does not affect
    /// single-step swap math when no tick crossing is simulated.
    #[inline]
    fn as_quote_state(&self) -> QuoteState {
        QuoteState {
            sqrt_price_x96: self.sqrt_price_x96,
            liquidity: self.liquidity,
            tick: self.tick,
            fee: self.fee,
        }
    }
}

/// Fast Uniswap V4 single-step simulator.
///
/// This delegates to the shared concentrated-liquidity quote engine.
/// Use this for cheap arbitrage pre-filtering.
/// Use a full simulator when tick crossing matters.
pub struct Pool;

impl Pool {
    /// Quote an exact-input swap.
    ///
    /// Returns output amount for the given input amount.
    ///
    /// `zero_for_one = true` means selling currency0 for currency1.
    #[inline]
    pub fn quote_exact_input(
        state: &PoolState,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<U256, Error> {
        Quoter::quote_exact_input(&state.as_quote_state(), amount_in, zero_for_one)
    }

    /// Quote an exact-output swap.
    ///
    /// Returns required input amount including fee.
    #[inline]
    pub fn quote_exact_output(
        state: &PoolState,
        amount_out: U256,
        zero_for_one: bool,
    ) -> Result<U256, Error> {
        Quoter::quote_exact_output(&state.as_quote_state(), amount_out, zero_for_one)
    }

    /// Return the full exact-input swap step.
    #[inline]
    pub fn step_exact_input(
        state: &PoolState,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<SwapStep, Error> {
        Quoter::step_exact_input(&state.as_quote_state(), amount_in, zero_for_one)
    }

    /// Return the full exact-output swap step.
    #[inline]
    pub fn step_exact_output(
        state: &PoolState,
        amount_out: U256,
        zero_for_one: bool,
    ) -> Result<SwapStep, Error> {
        Quoter::step_exact_output(&state.as_quote_state(), amount_out, zero_for_one)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_v4_pool() -> PoolState {
        PoolState {
            sqrt_price_x96: U256::ONE << 96,
            liquidity: 1_000_000_000_000_000_000u128,
            tick: 0,
            fee: 3000,
            tick_spacing: 60,
        }
    }

    #[test]
    fn v4_quote_exact_input_positive() {
        let pool = sample_v4_pool();

        let out = Pool::quote_exact_input(&pool, U256::from(1_000_000u64), true).unwrap();

        assert!(out > U256::ZERO);
    }

    #[test]
    fn v4_quote_exact_output_positive() {
        let pool = sample_v4_pool();

        let input = Pool::quote_exact_output(&pool, U256::from(1_000_000u64), true).unwrap();

        assert!(input > U256::ZERO);
    }

    #[test]
    fn v4_quote_matches_v3_for_identical_state() {
        let v4 = sample_v4_pool();
        let v3 = v4.as_quote_state();

        let amount_in = U256::from(1_000_000u64);

        assert_eq!(
            Pool::quote_exact_input(&v4, amount_in, true).unwrap(),
            Quoter::quote_exact_input(&v3, amount_in, true).unwrap(),
        );

        assert_eq!(
            Pool::quote_exact_input(&v4, amount_in, false).unwrap(),
            Quoter::quote_exact_input(&v3, amount_in, false).unwrap(),
        );
    }

    #[test]
    fn tick_spacing_does_not_affect_single_step_quote() {
        let base = sample_v4_pool();

        let pool_1 = PoolState {
            tick_spacing: 1,
            ..base
        };

        let pool_10 = PoolState {
            tick_spacing: 10,
            ..base
        };

        let pool_200 = PoolState {
            tick_spacing: 200,
            ..base
        };

        let amount_in = U256::from(1_000_000u64);

        let out_1 = Pool::quote_exact_input(&pool_1, amount_in, true);
        let out_10 = Pool::quote_exact_input(&pool_10, amount_in, true);
        let out_200 = Pool::quote_exact_input(&pool_200, amount_in, true);

        assert_eq!(out_1, out_10);
        assert_eq!(out_1, out_200);
    }

    #[test]
    fn step_exact_input_matches_quote() {
        let pool = sample_v4_pool();
        let amount_in = U256::from(1_000_000u64);

        let step = Pool::step_exact_input(&pool, amount_in, true).unwrap();
        let quoted = Pool::quote_exact_input(&pool, amount_in, true).unwrap();

        assert_eq!(step.amount_out, quoted);
        assert!(step.amount_in + step.fee_amount <= amount_in);
    }

    #[test]
    fn step_exact_output_matches_quote() {
        let pool = sample_v4_pool();
        let amount_out = U256::from(1_000_000u64);

        let step = Pool::step_exact_output(&pool, amount_out, false).unwrap();
        let quoted = Pool::quote_exact_output(&pool, amount_out, false).unwrap();

        assert_eq!(quoted, step.amount_in + step.fee_amount);
        assert!(step.amount_out <= amount_out);
    }
}
