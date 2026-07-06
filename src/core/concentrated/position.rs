use ruint::aliases::U256;

use crate::{Error, core::math::full};

pub type FeeGrowthX128 = U256;

/// Version-neutral fee accounting stored for one concentrated-liquidity position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PositionState {
    pub liquidity: u128,
    pub fee_growth_inside0_last_x128: FeeGrowthX128,
    pub fee_growth_inside1_last_x128: FeeGrowthX128,
}

impl PositionState {
    /// Apply Uniswap's `Position.update` accounting and return newly earned fees.
    pub fn update(
        &mut self,
        liquidity_delta: i128,
        fee_growth_inside0_x128: U256,
        fee_growth_inside1_x128: U256,
    ) -> Result<(U256, U256), Error> {
        if liquidity_delta == 0 && self.liquidity == 0 {
            return Err(Error::PositionNotFound);
        }

        let liquidity_before = self.liquidity;
        let liquidity_after = add_delta(liquidity_before, liquidity_delta)?;
        let fees0 = full::mul_div(
            fee_growth_inside0_x128.wrapping_sub(self.fee_growth_inside0_last_x128),
            U256::from(liquidity_before),
            U256::ONE << 128u32,
        )?;
        let fees1 = full::mul_div(
            fee_growth_inside1_x128.wrapping_sub(self.fee_growth_inside1_last_x128),
            U256::from(liquidity_before),
            U256::ONE << 128u32,
        )?;

        self.liquidity = liquidity_after;
        self.fee_growth_inside0_last_x128 = fee_growth_inside0_x128;
        self.fee_growth_inside1_last_x128 = fee_growth_inside1_x128;
        Ok((fees0, fees1))
    }
}

fn add_delta(liquidity: u128, delta: i128) -> Result<u128, Error> {
    if delta < 0 {
        liquidity
            .checked_sub(delta.unsigned_abs())
            .ok_or(Error::LiquidityUnderflow)
    } else {
        liquidity
            .checked_add(delta as u128)
            .ok_or(Error::LiquidityOverflow)
    }
}
