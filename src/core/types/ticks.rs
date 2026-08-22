use std::collections::BTreeMap;

use ruint::aliases::U256;
use serde::{Deserialize, Serialize};

use crate::{
    core::types::{tick::TickIndex, tick_spacing::TickSpacing},
    v4::SqrtPriceX96,
};

/// Tick data stored at an initialized tick boundary.
///
/// This intentionally mirrors Solidity's `mapping(int24 => Tick.Info)` shape:
/// the tick index is the `BTreeMap` key, not a duplicated field inside the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickInfo {
    pub sqrt_price_x96: SqrtPriceX96,

    pub tick_index: TickIndex,

    /// Gross liquidity attached to this initialized tick.
    pub liquidity_gross: u128,

    /// Signed liquidity delta applied when crossing this tick from left to right.
    pub liquidity_net: i128,

    /// Fee growth on the opposite side of this tick from the current price.
    pub fee_growth_outside0_x128: U256,

    /// Fee growth on the opposite side of this tick from the current price.
    pub fee_growth_outside1_x128: U256,
}

impl TickInfo {
    pub const DEFAULT: Self = Self {
        tick_index: TickIndex::MIN,
        sqrt_price_x96: SqrtPriceX96::MIN,
        liquidity_gross: 0,
        liquidity_net: 0,
        fee_growth_outside0_x128: U256::ZERO,
        fee_growth_outside1_x128: U256::ZERO,
    };

    pub fn new(tick_index: TickIndex, sqrt_price_x96: SqrtPriceX96) -> Self {
        Self {
            tick_index,
            sqrt_price_x96,
            liquidity_gross: 0,
            liquidity_net: 0,
            fee_growth_outside0_x128: U256::ZERO,
            fee_growth_outside1_x128: U256::ZERO,
        }
    }

    pub fn build(
        sqrt_price_x96: SqrtPriceX96,
        tick_index: TickIndex,
        liquidity_gross: u128,
        liquidity_net: i128,
        fee_growth_outside0_x128: U256,
        fee_growth_outside1_x128: U256,
    ) -> Self {
        Self {
            sqrt_price_x96,
            tick_index,
            liquidity_gross,
            liquidity_net,
            fee_growth_outside0_x128,
            fee_growth_outside1_x128,
        }
    }

    pub fn set_sqrt_price_x96(&mut self, sqrt_price_x96: SqrtPriceX96) {
        self.sqrt_price_x96 = sqrt_price_x96;
    }

    pub fn sqrt_price_x96(&self) -> SqrtPriceX96 {
        self.sqrt_price_x96
    }

    pub fn is_default(&self) -> bool {
        self == &Self::DEFAULT
    }

    pub fn is_liquidity_empty(&self) -> bool {
        self.liquidity_gross == 0
    }

    pub fn tick_index(&self) -> TickIndex {
        self.tick_index
    }
}

impl Default for TickInfo {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Result of locating the next initialized tick in the swap direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NextInitializedTick {
    pub tick_next: TickIndex,
    pub initialized: bool,
}

/// Serializable tick data stored in pool snapshots.
///
/// Snapshot maps already carry the tick index as their key, and the sqrt price
/// can be derived from that key, so the persisted value only keeps mutable
/// liquidity and fee-growth accounting fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickInfoSnapshot {
    /// Gross liquidity attached to this initialized tick.
    pub liquidity_gross: u128,

    /// Signed liquidity delta applied when crossing this tick from left to right.
    pub liquidity_net: i128,

    /// Fee growth on the opposite side of this tick from the current price.
    #[serde(default)]
    pub fee_growth_outside0_x128: U256,

    /// Fee growth on the opposite side of this tick from the current price.
    #[serde(default)]
    pub fee_growth_outside1_x128: U256,
}

impl TickInfoSnapshot {
    pub const DEFAULT: Self = Self {
        liquidity_gross: 0,
        liquidity_net: 0,
        fee_growth_outside0_x128: U256::ZERO,
        fee_growth_outside1_x128: U256::ZERO,
    };

    pub fn into_tick_info(self, tick_index: TickIndex) -> TickInfo {
        TickInfo::build(
            tick_index.sqrt_price_x96(),
            tick_index,
            self.liquidity_gross,
            self.liquidity_net,
            self.fee_growth_outside0_x128,
            self.fee_growth_outside1_x128,
        )
    }
}

impl Default for TickInfoSnapshot {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl From<TickInfo> for TickInfoSnapshot {
    fn from(info: TickInfo) -> Self {
        Self {
            liquidity_gross: info.liquidity_gross,
            liquidity_net: info.liquidity_net,
            fee_growth_outside0_x128: info.fee_growth_outside0_x128,
            fee_growth_outside1_x128: info.fee_growth_outside1_x128,
        }
    }
}

/// Serializable, owned snapshot of a pool's initialized ticks.
///
/// Unlike `PoolTicks::clone`, this value does not share mutable state with
/// the source pool. External snapshots are untrusted and must be rebuilt
/// through `PoolTicks::from_snapshot`, which validates every entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolTicksSnapshot {
    pub tick_spacing: TickSpacing,
    pub inner: BTreeMap<TickIndex, TickInfoSnapshot>,
}
