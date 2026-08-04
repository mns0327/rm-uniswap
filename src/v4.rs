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
pub use crate::core::concentrated::ticks::ticks::PoolTicks;
pub use crate::core::math::{
    full as full_math, sqrt_price as sqrt_price_math, swap as swap_math, swap::SwapStep,
    tick as tick_math,
};
pub use crate::core::types::delta::BalanceDelta;
pub use crate::core::types::signed::I256 as SignedAmount;
pub use crate::core::types::{PoolTicksSnapshot, TickInfo};

#[cfg(feature = "positions")]
pub mod positions {
    pub use crate::core::concentrated::position_manager::*;
}
