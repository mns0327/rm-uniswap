//! Keyed position storage for concentrated-liquidity accounting.
//!
//! Position storage for concentrated-liquidity accounting.
//!
//! The key is the owner, lower tick, upper tick, and salt, while the value
//! stores liquidity plus the last inside fee-growth checkpoints used to realize
//! accrued fees.

use std::collections::BTreeMap;

use ahash::AHashMap;
use alloy::primitives::{Address, FixedBytes};
use ruint::aliases::U256;
use serde::{Deserialize, Serialize};

use crate::{
    Error,
    core::{
        math::full,
        types::{PoolTicksSnapshot, liquidity::Liquidity, tick::TickIndex},
    },
};

/// Map key for one concentrated-liquidity position.
///
/// The same owner may hold multiple positions over the same tick range by using
/// a distinct `salt`. Tick validation is owned by the caller; this key stores
/// already-validated [`TickIndex`] values exactly as supplied.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionIndex {
    /// Account or manager address that owns the position.
    pub owner: Address,
    /// Inclusive lower tick of the position range.
    pub tick_lower: TickIndex,
    /// Exclusive upper tick of the position range.
    pub tick_upper: TickIndex,
    /// User-provided discriminator for otherwise identical owner/range pairs.
    pub salt: FixedBytes<32>,
}

/// Mutable accounting stored for one concentrated-liquidity position.
///
/// Fee-growth checkpoints are recorded when the position is last touched.
/// Comparing the current inside fee growth against these values yields the fees
/// accrued by the position's liquidity since that update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionState {
    /// Active liquidity owned by the position across its tick range.
    pub liquidity: Liquidity,
    /// Last token0 fee growth inside the position range, in X128 fixed point.
    pub fee_growth_inside0_last_x128: U256,
    /// Last token1 fee growth inside the position range, in X128 fixed point.
    pub fee_growth_inside1_last_x128: U256,
}

/// In-memory position map keyed by [`PositionIndex`].
///
/// The wrapper keeps the storage type explicit while allowing callers to depend
/// on the position-access abstraction instead of a concrete hash-map
/// implementation.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Positions(pub AHashMap<PositionIndex, PositionState>);

/// Abstraction over optional position storage.
///
/// Implemented by [`Positions`] when per-position accounting is enabled and by
/// `()` when a caller wants a no-op store for quote-only or pool-only paths.
pub trait PositionsAccess: Default {
    /// Whether this implementation actually persists position state.
    const ENABLE: bool;

    /// Returns mutable state for `index`, if present.
    fn update_position<F, T>(&mut self, index: PositionIndex, f: F) -> T
    where
        F: FnOnce(&mut PositionState) -> T;

    /// Validates position-derived liquidity against initialized tick state.
    ///
    /// No-op stores have no position data to compare. Concrete stores should
    /// reject snapshots where the position map and tick liquidity can diverge.
    fn validate_against_ticks(&self, _ticks: &PoolTicksSnapshot) -> Result<(), Error> {
        Ok(())
    }
}

impl PositionState {
    pub const DEFAULT: Self = PositionState {
        liquidity: Liquidity::ZERO,
        fee_growth_inside0_last_x128: U256::ZERO,
        fee_growth_inside1_last_x128: U256::ZERO,
    };

    /// Apply Uniswap's `Position.update` accounting and return newly earned fees.
    pub fn update(
        &mut self,
        liquidity_delta: i128,
        fee_growth_inside0_x128: U256,
        fee_growth_inside1_x128: U256,
    ) -> Result<(U256, U256), Error> {
        let liquidity_before = self.liquidity;

        if liquidity_delta == 0 {
            if self.liquidity.is_zero() {
                return Err(Error::PositionNotFound);
            }
        } else {
            self.liquidity = self
                .liquidity
                .checked_add_signed(liquidity_delta)
                .ok_or(Error::LiquidityOverflow)?
                .into();
        }

        let (fees_owed0, fees_owed1) = if !liquidity_before.is_zero() {
            let fees_owed0 = full::mul_div(
                fee_growth_inside0_x128.wrapping_sub(self.fee_growth_inside0_last_x128),
                liquidity_before.as_u256(),
                U256::ONE << 128u32,
            )?;
            let fees_owed1 = full::mul_div(
                fee_growth_inside1_x128.wrapping_sub(self.fee_growth_inside1_last_x128),
                liquidity_before.as_u256(),
                U256::ONE << 128u32,
            )?;
            (fees_owed0, fees_owed1)
        } else {
            (U256::ZERO, U256::ZERO)
        };

        self.fee_growth_inside0_last_x128 = fee_growth_inside0_x128;
        self.fee_growth_inside1_last_x128 = fee_growth_inside1_x128;

        Ok((fees_owed0, fees_owed1))
    }
}

impl Default for PositionState {
    #[inline(always)]
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[allow(dead_code)]
impl Positions {
    /// Creates an empty position store.
    #[inline(always)]
    pub fn new() -> Self {
        Self(AHashMap::new())
    }
}

impl PositionsAccess for Positions {
    /// Concrete stores retain position state.
    const ENABLE: bool = true;

    #[inline(always)]
    fn update_position<F, T>(&mut self, index: PositionIndex, f: F) -> T
    where
        F: FnOnce(&mut PositionState) -> T,
    {
        let position = self.0.entry(index).or_insert(PositionState::DEFAULT);

        f(position)
    }

    fn validate_against_ticks(&self, ticks: &PoolTicksSnapshot) -> Result<(), Error> {
        let mut expected = BTreeMap::<TickIndex, (u128, i128)>::new();

        for (index, position) in &self.0 {
            if index.tick_lower >= index.tick_upper
                || !index.tick_lower.for_spacing(ticks.tick_spacing)
                || !index.tick_upper.for_spacing(ticks.tick_spacing)
            {
                return Err(Error::InvalidTick);
            }

            let liquidity = position.liquidity.value();
            if liquidity == 0 {
                continue;
            }

            let liquidity_delta =
                i128::try_from(liquidity).map_err(|_| Error::LiquidityOverflow)?;

            let lower = expected.entry(index.tick_lower).or_default();
            lower.0 = lower
                .0
                .checked_add(liquidity)
                .ok_or(Error::LiquidityOverflow)?;
            lower.1 = lower
                .1
                .checked_add(liquidity_delta)
                .ok_or(Error::LiquidityOverflow)?;

            let upper = expected.entry(index.tick_upper).or_default();
            upper.0 = upper
                .0
                .checked_add(liquidity)
                .ok_or(Error::LiquidityOverflow)?;
            upper.1 = upper
                .1
                .checked_sub(liquidity_delta)
                .ok_or(Error::LiquidityOverflow)?;
        }

        let actual = ticks.inner.iter().map(|(&tick_idx, tick)| {
            (tick_idx, (tick.liquidity_gross.value(), tick.liquidity_net))
        });
        if !expected.into_iter().eq(actual) {
            return Err(Error::InvalidTick);
        }

        Ok(())
    }
}

impl Default for Positions {
    #[inline(always)]
    fn default() -> Self {
        Self::new()
    }
}

impl PositionsAccess for () {
    /// The unit implementation intentionally drops all position state.
    const ENABLE: bool = false;

    #[inline(always)]
    fn update_position<F, T>(&mut self, _: PositionIndex, f: F) -> T
    where
        F: FnOnce(&mut PositionState) -> T,
    {
        let mut position = PositionState::DEFAULT;
        f(&mut position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::core::types::{
        liquidity::Liquidity, tick::TickIndex, tick_spacing::TickSpacing, ticks::TickInfoInner,
    };

    fn tick(index: i32) -> TickIndex {
        TickIndex::new(index).expect("valid test tick")
    }

    fn tick_spacing_60() -> TickSpacing {
        TickSpacing::new(60).expect("valid spacing")
    }

    fn position_index(owner_byte: u8, lower: i32, upper: i32) -> PositionIndex {
        PositionIndex {
            owner: Address::repeat_byte(owner_byte),
            tick_lower: tick(lower),
            tick_upper: tick(upper),
            salt: FixedBytes::<32>::repeat_byte(owner_byte),
        }
    }

    fn position_state(liquidity: u128) -> PositionState {
        PositionState {
            liquidity: Liquidity::new(liquidity),
            ..PositionState::DEFAULT
        }
    }

    fn tick_snapshot(gross: u128, net: i128) -> TickInfoInner {
        TickInfoInner {
            liquidity_gross: Liquidity::new(gross),
            liquidity_net: net,
            ..TickInfoInner::DEFAULT
        }
    }

    #[test]
    fn validates_positions_against_matching_tick_liquidity() {
        let positions = Positions(AHashMap::from_iter([
            (position_index(0x11, -120, 120), position_state(100)),
            (position_index(0x22, -120, 60), position_state(40)),
        ]));
        let ticks = PoolTicksSnapshot {
            tick_spacing: tick_spacing_60(),
            inner: BTreeMap::from([
                (tick(-120), tick_snapshot(140, 140)),
                (tick(60), tick_snapshot(40, -40)),
                (tick(120), tick_snapshot(100, -100)),
            ]),
        };

        assert_eq!(positions.validate_against_ticks(&ticks), Ok(()));
    }

    #[test]
    fn rejects_tick_liquidity_that_does_not_match_positions() {
        let positions = Positions(AHashMap::from_iter([(
            position_index(0x11, -120, 120),
            position_state(100),
        )]));
        let ticks = PoolTicksSnapshot {
            tick_spacing: tick_spacing_60(),
            inner: BTreeMap::from([
                (tick(-120), tick_snapshot(90, 90)),
                (tick(120), tick_snapshot(100, -100)),
            ]),
        };

        assert_eq!(
            positions.validate_against_ticks(&ticks),
            Err(Error::InvalidTick)
        );
    }
}
