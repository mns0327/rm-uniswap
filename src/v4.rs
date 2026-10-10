//! Uniswap v4-specific facade.
//!
//! Shared concentrated-liquidity pool, tick, fee, and math types are exported
//! from the crate root. This module contains v4-only surfaces such as hooks,
//! pool keys, and the single-pool manager adapter.
pub use crate::core::types::delta::BeforeSwapDelta;
pub use crate::core::types::fee::DYNAMIC_FEE_FLAG;
pub use crate::core::types::hooks::{HookEnv, Hooks, HooksImpl};
pub use crate::core::types::pool_key::{PoolId, PoolKey};

pub mod stable_pair;

pub mod positions {
    pub use crate::core::concentrated::pool_manager::*;
    pub use crate::core::concentrated::position::{PositionIndex, Positions};
}
