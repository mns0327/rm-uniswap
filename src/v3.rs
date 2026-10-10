//! Uniswap v3-specific facade.
//!
//! The current crate-level API exposes the shared concentrated-liquidity pool,
//! tick, fee, and math types used by both v3-style and v4-style integrations.
//! This module is reserved for types that are specific to v3 and should not be
//! shared with the v4 facade.

use alloy::primitives::{Address, FixedBytes, I256};
use ruint::aliases::U256;

use crate::{
    Error, Liquidity, ModifyLiquidityParams, SqrtPriceX96, SwapFee, SwapParamsInner, TickIndex,
    TickSpacing, core::concentrated, v4::positions::Positions,
};

pub struct Pool {
    pool: concentrated::pool::Pool<Positions>,
}

impl Pool {
    #[inline]
    pub fn new(
        sqrt_price_x96: SqrtPriceX96,
        tick: TickIndex,
        liquidity: Liquidity,
        swap_fee: SwapFee,
        tick_spacing: TickSpacing,
    ) -> Self {
        Self {
            pool: concentrated::pool::Pool::new(
                sqrt_price_x96,
                tick,
                liquidity,
                swap_fee,
                tick_spacing,
            ),
        }
    }

    pub fn mint(
        &mut self,
        owner: Address,
        tick_lower: TickIndex,
        tick_upper: TickIndex,
        amount: u128,
    ) -> Result<(U256, U256), Error> {
        let result = self.pool.modify_liquidity(ModifyLiquidityParams {
            tick_lower,
            tick_upper,
            liquidity_delta: amount as i128,
            owner,
            salt: FixedBytes::default(),
        })?;

        Ok((
            U256::from(result.delta.amount0),
            U256::from(result.delta.amount1),
        ))
    }

    pub fn burn(
        &mut self,
        owner: Address,
        tick_lower: TickIndex,
        tick_upper: TickIndex,
        amount: u128,
    ) -> Result<(U256, U256), Error> {
        let result = self.pool.modify_liquidity(ModifyLiquidityParams {
            tick_lower,
            tick_upper,
            liquidity_delta: -(amount as i128),
            owner,
            salt: FixedBytes::default(),
        })?;

        Ok((
            U256::from(-result.delta.amount0) + U256::from(result.fee_delta.amount0),
            U256::from(-result.delta.amount1) + U256::from(result.fee_delta.amount1),
        ))
    }

    pub fn swap(
        &mut self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: SqrtPriceX96,
    ) -> Result<(I256, I256), Error> {
        let result = self.pool.swap(SwapParamsInner {
            zero_for_one,
            amount_specified,
            sqrt_price_limit,
            lp_fee_override: None,
        })?;

        Ok((
            I256::try_from(result.swap_delta.amount0).expect("i128 fits in I256"),
            I256::try_from(result.swap_delta.amount1).expect("i128 fits in I256"),
        ))
    }
}
