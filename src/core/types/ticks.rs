use std::collections::BTreeMap;

use ruint::aliases::U256;
use serde::{Deserialize, Serialize};

/// Shared initialized-tick representation used by concentrated-liquidity pools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickEntry {
    pub tick_idx: i32,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
}

/// Tick data stored at an initialized tick boundary.
///
/// This intentionally mirrors Solidity's `mapping(int24 => Tick.Info)` shape:
/// the tick index is the `BTreeMap` key, not a duplicated field inside the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TickInfo {
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

impl From<TickEntry> for TickInfo {
    #[inline]
    fn from(entry: TickEntry) -> Self {
        Self {
            liquidity_gross: entry.liquidity_gross,
            liquidity_net: entry.liquidity_net,
            fee_growth_outside0_x128: U256::ZERO,
            fee_growth_outside1_x128: U256::ZERO,
        }
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
