use std::collections::BTreeMap;

use ruint::aliases::U256;
use serde::{Deserialize, Serialize};

/// Tick data stored at an initialized tick boundary.
///
/// This intentionally mirrors Solidity's `mapping(int24 => Tick.Info)` shape:
/// the tick index is the `BTreeMap` key, not a duplicated field inside the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TickInfo {
    /// TODO: Add Sqrt price
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
        liquidity_gross: 0,
        liquidity_net: 0,
        fee_growth_outside0_x128: U256::ZERO,
        fee_growth_outside1_x128: U256::ZERO,
    };

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
    pub tick_next: i32,
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
    pub tick_spacing: i32,
    pub inner: BTreeMap<i32, TickInfo>,
}
