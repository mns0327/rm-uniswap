//! Tick payload and snapshot types for initialized pool boundaries.
//!
//! Runtime tick storage keeps derived values such as the tick's sqrt price next
//! to the mutable liquidity and fee accounting fields so swap traversal can use
//! one compact payload. Snapshots keep only the fields that cannot be derived
//! from the map key, then rebuild full [`TickInfo`] values when loaded into a
//! tick store.

use std::collections::BTreeMap;

use ruint::aliases::U256;
use serde::{Deserialize, Serialize};

use crate::{
    core::types::{tick::TickIndex, tick_spacing::TickSpacing},
    v4::{Liquidity, SqrtPriceX96},
};

/// Tick data stored at an initialized tick boundary.
///
/// The liquidity and fee-growth fields mirror Uniswap V4's `Tick.Info`
/// accounting. This runtime form also stores the validated tick index and its
/// Q64.96 sqrt price so swap steps can jump to a boundary without recomputing
/// the price from the map key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickInfo {
    /// Sqrt price for this boundary tick, in Q64.96 fixed-point.
    pub sqrt_price_x96: SqrtPriceX96,

    /// Validated protocol tick represented by this initialized boundary.
    pub tick_index: TickIndex,

    /// Gross liquidity attached to this initialized tick.
    ///
    /// This is the absolute liquidity present at the boundary across all
    /// positions using it. A tick with zero gross liquidity is no longer
    /// considered initialized and should be removed from the tick store.
    pub liquidity_gross: Liquidity,

    /// Signed liquidity delta applied when crossing this tick from left to right.
    ///
    /// Lower position ticks add liquidity and upper position ticks remove it.
    /// Swap code negates this value when crossing right to left.
    pub liquidity_net: i128,

    /// Fee growth on the opposite side of this tick from the current price.
    ///
    /// Crossing a tick flips this value around the token0 global fee-growth
    /// accumulator with wrapping `U256` arithmetic.
    pub fee_growth_outside0_x128: U256,

    /// Fee growth on the opposite side of this tick from the current price.
    ///
    /// Crossing a tick flips this value around the token1 global fee-growth
    /// accumulator with wrapping `U256` arithmetic.
    pub fee_growth_outside1_x128: U256,
}

impl TickInfo {
    /// Placeholder tick payload used by fixed-size slab pages.
    ///
    /// `TickSlabValue` tracks logical initialization with a separate occupancy
    /// bitmap, so this value may sit in unused slots without representing an
    /// initialized tick.
    pub const DEFAULT: Self = Self {
        tick_index: TickIndex::MIN,
        sqrt_price_x96: SqrtPriceX96::MIN,
        liquidity_gross: Liquidity::ZERO,
        liquidity_net: 0,
        fee_growth_outside0_x128: U256::ZERO,
        fee_growth_outside1_x128: U256::ZERO,
    };

    /// Creates an empty initialized-boundary payload for `tick_index`.
    ///
    /// Liquidity and fee-growth accounting start at zero. Pool liquidity updates
    /// fill the gross/net fields before the tick becomes meaningful to swaps.
    #[inline(always)]
    pub const fn new(tick_index: TickIndex, sqrt_price_x96: SqrtPriceX96) -> Self {
        Self {
            tick_index,
            sqrt_price_x96,
            liquidity_gross: Liquidity::ZERO,
            liquidity_net: 0,
            fee_growth_outside0_x128: U256::ZERO,
            fee_growth_outside1_x128: U256::ZERO,
        }
    }

    /// Builds a full runtime tick payload from already validated components.
    ///
    /// This constructor is used when restoring snapshots or assembling trusted
    /// fixtures. It does not validate that `sqrt_price_x96` matches
    /// `tick_index`; callers should derive both from the same tick or validate
    /// snapshot state before constructing a live pool.
    #[inline(always)]
    pub fn build(
        sqrt_price_x96: SqrtPriceX96,
        tick_index: TickIndex,
        liquidity_gross: Liquidity,
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

    /// Replaces the cached sqrt price for this tick.
    ///
    /// Callers should only use a price derived from this tick's [`TickIndex`].
    #[inline(always)]
    pub fn set_sqrt_price_x96(&mut self, sqrt_price_x96: SqrtPriceX96) {
        self.sqrt_price_x96 = sqrt_price_x96;
    }

    /// Returns the cached Q64.96 sqrt price for this boundary tick.
    #[inline(always)]
    pub fn sqrt_price_x96(&self) -> SqrtPriceX96 {
        self.sqrt_price_x96
    }

    /// Returns whether this value is the slab placeholder payload.
    ///
    /// Logical initialization is owned by the tick store, not by this comparison.
    #[inline(always)]
    pub fn is_default(&self) -> bool {
        self == &Self::DEFAULT
    }

    /// Returns whether no gross liquidity is attached to this boundary.
    ///
    /// Tick stores use this condition to decide when a boundary can be removed.
    #[inline(always)]
    pub fn is_liquidity_empty(&self) -> bool {
        self.liquidity_gross.is_zero()
    }

    /// Returns the validated protocol tick represented by this payload.
    #[inline(always)]
    pub fn tick_index(&self) -> TickIndex {
        self.tick_index
    }
}

impl Default for TickInfo {
    #[inline(always)]
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Serializable tick data stored in pool snapshots.
///
/// Snapshot maps already carry the tick index as their key, and the sqrt price
/// can be derived from that key, so the persisted value only keeps mutable
/// liquidity and fee-growth accounting fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickInfoSnapshot {
    /// Gross liquidity attached to this initialized tick.
    ///
    /// Snapshot validation rejects zero gross liquidity because persisted tick
    /// maps should contain only initialized boundaries.
    pub liquidity_gross: Liquidity,

    /// Signed liquidity delta applied when crossing this tick from left to right.
    ///
    /// Valid snapshots require `abs(liquidity_net) <= liquidity_gross`.
    pub liquidity_net: i128,

    /// Fee growth on the opposite side of this tick from the current price.
    ///
    /// Defaults to zero for backward-compatible snapshots that predate
    /// fee-growth-outside persistence.
    #[serde(default)]
    pub fee_growth_outside0_x128: U256,

    /// Fee growth on the opposite side of this tick from the current price.
    ///
    /// Defaults to zero for backward-compatible snapshots that predate
    /// fee-growth-outside persistence.
    #[serde(default)]
    pub fee_growth_outside1_x128: U256,
}

impl TickInfoSnapshot {
    /// Empty snapshot payload used as a construction default in tests and fixtures.
    pub const DEFAULT: Self = Self {
        liquidity_gross: Liquidity::ZERO,
        liquidity_net: 0,
        fee_growth_outside0_x128: U256::ZERO,
        fee_growth_outside1_x128: U256::ZERO,
    };

    /// Restores this snapshot into the runtime tick payload for `tick_index`.
    ///
    /// The sqrt price is derived from the map key so serialized snapshots do not
    /// need to persist redundant price data.
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

/// Drops derived runtime fields when writing an initialized tick to a snapshot.
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
/// Unlike a live tick store, this value owns plain serializable data and shares
/// no mutable storage with the source pool. External snapshots are untrusted:
/// rebuild them through the tick-store snapshot loader so spacing alignment,
/// non-zero gross liquidity, and net-liquidity bounds are validated before use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolTicksSnapshot {
    /// Tick spacing used to interpret every key in `inner`.
    pub tick_spacing: TickSpacing,

    /// Initialized tick snapshots keyed by validated protocol tick.
    ///
    /// `BTreeMap` keeps snapshots deterministic and ordered by protocol tick.
    pub inner: BTreeMap<TickIndex, TickInfoSnapshot>,
}
