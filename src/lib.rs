use std::fmt::Debug;

use alloy::primitives::U256;
use arb_types::sqrt_price_x96_to_price;

use crate::v3::{
    error::SwapSimError,
    pool::{
        FullSwapResult, ModifyLiquidityParams, ModifyLiquidityResult, SwapParams,
        SwapSimulationResult,
    },
    Pool,
};

pub mod v2;
pub mod v3;
pub mod v4;

pub use v3::pool::PoolState;

pub trait PoolTrait: Debug + Send + Sync {
    fn set_pool_state(&self, sqrt_price_x96: U256, tick: i32, liquidity: u128);
    fn sqrt_price_x96(&self) -> U256;
    fn tick(&self) -> i32;
    fn pool(&self) -> &Pool;
    fn liquidity(&self) -> u128;
    fn fee(&self) -> u32;
    fn price(&self) -> f64;
    fn price_with_fee(&self, zero_for_one: bool) -> f64;
    fn swap(&self, params: SwapParams) -> Result<FullSwapResult, SwapSimError>;
    fn simulate_swap(&self, params: SwapParams) -> Result<SwapSimulationResult, SwapSimError>;
    fn simulate_swap_full(&self, params: SwapParams) -> Result<FullSwapResult, SwapSimError>;
    fn modify_liquidity(
        &self,
        params: ModifyLiquidityParams,
    ) -> Result<ModifyLiquidityResult, SwapSimError>;
}

impl PoolTrait for v3::Pool {
    fn set_pool_state(&self, sqrt_price_x96: U256, tick: i32, liquidity: u128) {
        {
            let mut state = self.state.write();
            state.sqrt_price_x96 = sqrt_price_x96;
            state.tick = tick;
            state.liquidity = liquidity;
        }

        self.price_cache
            .update_price(sqrt_price_x96_to_price(sqrt_price_x96));

        self.cache.clear_cross_amounts();
    }

    fn pool(&self) -> &Pool {
        self
    }

    fn sqrt_price_x96(&self) -> U256 {
        let state = self.state.read();
        state.sqrt_price_x96
    }

    fn tick(&self) -> i32 {
        let state = self.state.read();
        state.tick
    }

    fn liquidity(&self) -> u128 {
        let state = self.state.read();
        state.liquidity
    }

    fn fee(&self) -> u32 {
        self.fee
    }

    fn price(&self) -> f64 {
        let state = self.state.read();

        sqrt_price_x96_to_price(state.sqrt_price_x96)
    }

    fn price_with_fee(&self, zero_for_one: bool) -> f64 {
        self.price_cache.get_price_with_fee(zero_for_one)
    }

    #[inline]
    fn swap(&self, params: SwapParams) -> Result<FullSwapResult, SwapSimError> {
        let result = self.swap(params);

        self.price_cache
            .update_price(sqrt_price_x96_to_price(self.sqrt_price_x96()));

        result
    }

    #[inline]
    fn simulate_swap(&self, params: SwapParams) -> Result<SwapSimulationResult, SwapSimError> {
        self.simulate_swap(params)
    }

    #[inline]
    fn simulate_swap_full(&self, params: SwapParams) -> Result<FullSwapResult, SwapSimError> {
        self.simulate_swap_full(params)
    }

    #[inline]
    fn modify_liquidity(
        &self,
        params: ModifyLiquidityParams,
    ) -> Result<ModifyLiquidityResult, SwapSimError> {
        self.modify_liquidity(params)
    }
}
