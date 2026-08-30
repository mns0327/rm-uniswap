/// Uniswap V4-compatible facade.
///
/// The public API mirrors the Solidity library names while using Rust method
/// names: [`FullMath`], [`SqrtPriceMath`], [`SwapMath`], and [`TickMath`].
///
/// Use [`Quoter`] for single-range quotes that cannot cross an initialized
/// tick. Use [`Pool`] when a swap may cross ticks or when committed pool state
/// must be updated.
pub use crate::Error;
pub use crate::core::concentrated::pool::{Pool, PoolSnapshot, TickCrossInfo};
pub use crate::core::concentrated::ticks::slab::TickSlab as PoolTicks;
pub use crate::core::math::{
    full as full_math, sqrt_price as sqrt_price_math, swap as swap_math, swap::SwapStep,
    tick as tick_math,
};
pub use crate::core::types::delta::BalanceDelta;
pub use crate::core::types::fee::Fee;
pub use crate::core::types::fee::protocol_fee::ProtocolFee;
pub use crate::core::types::fee::swap_fee::SwapFee;
pub use crate::core::types::hooks::Hooks;
pub use crate::core::types::liquidity::Liquidity;
pub use crate::core::types::nonzero::NonZeroLiquidity;
pub use crate::core::types::params::{
    ModifyLiquidityParams, ModifyLiquidityResult, SwapParams, SwapResult, SwapSimulationResult,
};
pub use crate::core::types::pool_key::{PoolId, PoolKey};
pub use crate::core::types::pool_state::PoolState;
pub use crate::core::types::sqrt_price::SqrtPriceX96;
pub use crate::core::types::tick::TickIndex;
pub use crate::core::types::tick_spacing::TickSpacing;
pub use crate::core::types::{PoolTicksSnapshot, TickInfo, TickInfoSnapshot};

#[cfg(feature = "positions")]
pub mod positions {
    pub use crate::core::concentrated::position_manager::*;
}
