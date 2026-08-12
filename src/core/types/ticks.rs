use std::collections::BTreeMap;

use ruint::aliases::{U160, U256};
use serde::{Deserialize, Serialize};

use crate::core::types::{tick::TickIndex, tick_spacing::TickSpacing};

/// Tick data stored at an initialized tick boundary.
///
/// This intentionally mirrors Solidity's `mapping(int24 => Tick.Info)` shape:
/// the tick index is the `BTreeMap` key, not a duplicated field inside the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TickInfo {
    /// The square root of the price at this tick, scaled by 96 bits.
    #[serde(default)]
    pub sqrt_price_x96: U160,

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

impl TickInfo {
    pub const DEFAULT: Self = Self {
        sqrt_price_x96: U160::ZERO,
        liquidity_gross: 0,
        liquidity_net: 0,
        fee_growth_outside0_x128: U256::ZERO,
        fee_growth_outside1_x128: U256::ZERO,
    };

    pub fn new(
        sqrt_price_x96: U160,
        liquidity_gross: u128,
        liquidity_net: i128,
        fee_growth_outside0_x128: U256,
        fee_growth_outside1_x128: U256,
    ) -> Self {
        Self {
            sqrt_price_x96,
            liquidity_gross,
            liquidity_net,
            fee_growth_outside0_x128,
            fee_growth_outside1_x128,
        }
    }

    pub fn set_sqrt_price_x96(&mut self, sqrt_price_x96: U160) {
        self.sqrt_price_x96 = sqrt_price_x96;
    }

    pub fn sqrt_price_x96(&self) -> U160 {
        self.sqrt_price_x96
    }

    pub fn is_default(&self) -> bool {
        self == &Self::DEFAULT
    }

    pub fn is_liquidity_empty(&self) -> bool {
        self.liquidity_gross == 0
    }
}

/// Result of locating the next initialized tick in the swap direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NextInitializedTick {
    pub tick_next: TickIndex,
    pub initialized: bool,
}

/// Result of applying a liquidity delta to a tick boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickUpdate {
    /// Whether the tick flipped initialized/uninitialized state.
    pub flipped: bool,
    /// Gross liquidity after the update.
    pub liquidity_gross_after: u128,
}

/// Serializable, owned snapshot of a pool's initialized ticks.
///
/// Unlike `PoolTicks::clone`, this value does not share mutable state with
/// the source pool. External snapshots are untrusted and must be rebuilt
/// through `PoolTicks::from_snapshot`, which validates every entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolTicksSnapshot {
    pub tick_spacing: TickSpacing,
    pub inner: BTreeMap<TickIndex, TickInfo>,
}
