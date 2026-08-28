//! Pool-state storage adapters for concentrated-liquidity swap execution.
//!
//! [`PoolState`] holds the mutable pool fields that change as swaps advance.
//! [`StateAccess`] lets the shared swap loop operate on either a read-only
//! snapshot for quotes or a mutable state reference for live swaps.

use ruint::aliases::U256;
use serde::{Deserialize, Serialize};

use crate::v4::{Liquidity, SqrtPriceX96, TickIndex};

/// Current V4 pool state required by the concentrated-liquidity swap loop.
///
/// The price, tick, and active liquidity describe the current position inside
/// the initialized tick grid. Fee-growth globals track accumulated LP fees per
/// unit of active liquidity for each token and are updated only for the input
/// token of a successful swap.
///
/// # Invariants
///
/// * `tick` must match the floor tick for `sqrt_price_x96`.
/// * `liquidity` must equal the net active liquidity at `tick`.
/// * Fee-growth values use Uniswap's Q128 accumulator semantics and may wrap
///   modulo 2^256, matching on-chain arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolState {
    /// Current pool sqrt price in Q64.96 fixed-point.
    pub sqrt_price_x96: SqrtPriceX96,

    /// Current tick — must equal `floor(log_√1.0001(sqrt_price_x96))`.
    pub tick: TickIndex,

    /// Active liquidity for the current tick range (`uint128` in V4).
    pub liquidity: Liquidity,

    /// All-time LP fee growth per unit of active liquidity in token0, Q128.
    #[serde(default)]
    pub fee_growth_global0_x128: U256,

    /// All-time LP fee growth per unit of active liquidity in token1, Q128.
    #[serde(default)]
    pub fee_growth_global1_x128: U256,
}

/// Minimal pool-state interface needed by the shared swap loop.
///
/// [`Pool::quote_swap`](crate::core::concentrated::pool::Pool::quote_swap) uses
/// the `&PoolState` implementation to read an initial snapshot and discard the
/// simulated final state. [`Pool::swap`](crate::core::concentrated::pool::Pool::swap)
/// uses the `&mut PoolState` implementation to commit the final tick, price,
/// liquidity, and updated fee-growth accumulator after the loop succeeds.
pub trait StateAccess {
    /// Returns the pool state snapshot used as the swap loop's starting point.
    fn get(&self) -> PoolState;

    /// Commits the final pool state produced by the swap loop.
    ///
    /// `fee_growth_global_x128` belongs to the swap input token. `zero_for_one`
    /// identifies that token, so implementations update token0 fee growth for
    /// token0-to-token1 swaps and token1 fee growth for the opposite direction.
    fn set(
        &mut self,
        tick_idx: TickIndex,
        sqrt_price_x96: SqrtPriceX96,
        liquidity: Liquidity,
        zero_for_one: bool,
        fee_growth_global_x128: U256,
    );
}

/// Read-only state access for quote simulations.
///
/// Quotes need the same starting state as live swaps, but they intentionally
/// ignore the final state passed to [`StateAccess::set`].
impl StateAccess for &PoolState {
    #[inline(always)]
    fn get(&self) -> PoolState {
        PoolState {
            tick: self.tick,
            sqrt_price_x96: self.sqrt_price_x96,
            liquidity: self.liquidity,
            fee_growth_global0_x128: self.fee_growth_global0_x128,
            fee_growth_global1_x128: self.fee_growth_global1_x128,
        }
    }

    #[inline(always)]
    fn set(&mut self, _: TickIndex, _: SqrtPriceX96, _: Liquidity, _: bool, _: U256) {}
}

/// Mutating state access for live swaps.
///
/// The shared swap loop calls [`StateAccess::set`] once, after all swap math and
/// tick crossings have succeeded, so the pool state is committed as one final
/// snapshot.
impl StateAccess for &mut PoolState {
    #[inline(always)]
    fn get(&self) -> PoolState {
        PoolState {
            tick: self.tick,
            sqrt_price_x96: self.sqrt_price_x96,
            liquidity: self.liquidity,
            fee_growth_global0_x128: self.fee_growth_global0_x128,
            fee_growth_global1_x128: self.fee_growth_global1_x128,
        }
    }

    #[inline(always)]
    fn set(
        &mut self,
        tick_idx: TickIndex,
        sqrt_price_x96: SqrtPriceX96,
        liquidity: Liquidity,
        zero_for_one: bool,
        fee_growth_global_x128: U256,
    ) {
        self.tick = tick_idx;
        self.sqrt_price_x96 = sqrt_price_x96;
        self.liquidity = liquidity;

        if !zero_for_one {
            self.fee_growth_global1_x128 = fee_growth_global_x128;
        } else {
            self.fee_growth_global0_x128 = fee_growth_global_x128;
        }
    }
}
