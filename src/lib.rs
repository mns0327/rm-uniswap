use std::fmt::Debug;

use alloy::primitives::U256;

use crate::v3::{
    Pool,
    error::SwapSimError,
    pool::{
        FullSwapResult, ModifyLiquidityParams, ModifyLiquidityResult, SwapParams,
        SwapSimulationResult,
    },
};

pub mod v2;
pub mod v3;
pub mod v4;
