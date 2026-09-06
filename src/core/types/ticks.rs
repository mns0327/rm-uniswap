//! Tick payload and snapshot types for initialized pool boundaries.
//!
//! [`TickInfo`] is the live storage form used by swap traversal. It keeps
//! derived boundary metadata next to the mutable liquidity and fee accounting
//! payload so the tick store can return one compact value. Serializable
//! snapshots reuse [`TickInfoInner`] because the map key already carries the
//! tick index, and the sqrt price can be derived from that key when a live
//! store is rebuilt.

use std::collections::BTreeMap;

use ruint::aliases::U256;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::core::types::{
    liquidity::Liquidity, sqrt_price::SqrtPriceX96, tick::TickIndex, tick_spacing::TickSpacing,
};

/// Tick data stored at an initialized tick boundary.
///
/// The liquidity and fee-growth fields mirror Uniswap-style tick accounting
/// accounting. This runtime form also stores the validated tick index and its
/// Q64.96 sqrt price so swap steps can jump to a boundary without recomputing
/// the price from the map key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickInfo {
    /// Sqrt price for this boundary tick, in Q64.96 fixed-point.
    pub sqrt_price_x96: SqrtPriceX96,

    /// Validated protocol tick represented by this initialized boundary.
    pub tick_index: TickIndex,

    /// Liquidity and fee-growth accounting for this initialized boundary.
    pub inner: TickInfoInner,
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
        inner: TickInfoInner::DEFAULT,
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
            inner: TickInfoInner::DEFAULT,
        }
    }

    /// Creates a runtime tick payload from prebuilt accounting fields.
    ///
    /// This is the const-friendly constructor for data that already carries
    /// validated tick metadata, such as slab restoration and static fixtures.
    /// It does not validate that `sqrt_price_x96` matches `tick_index`.
    #[inline(always)]
    pub const fn with_inner(
        tick_index: TickIndex,
        sqrt_price_x96: SqrtPriceX96,
        inner: TickInfoInner,
    ) -> Self {
        Self {
            tick_index,
            sqrt_price_x96,
            inner,
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
            inner: TickInfoInner {
                liquidity_gross,
                liquidity_net,
                fee_growth_outside0_x128,
                fee_growth_outside1_x128,
            },
        }
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
        self.inner.liquidity_gross.is_zero()
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

/// Liquidity and fee-growth accounting stored for an initialized tick.
///
/// This is the mutable accounting payload shared by live tick storage and
/// serializable snapshots. Runtime storage wraps it in [`TickInfo`] to attach
/// the validated tick index and derived sqrt price needed during swap
/// traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TickInfoInner {
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
    #[cfg_attr(feature = "serde", serde(default))]
    pub fee_growth_outside0_x128: U256,

    /// Fee growth on the opposite side of this tick from the current price.
    ///
    /// Crossing a tick flips this value around the token1 global fee-growth
    /// accumulator with wrapping `U256` arithmetic.
    #[cfg_attr(feature = "serde", serde(default))]
    pub fee_growth_outside1_x128: U256,
}

impl TickInfoInner {
    /// Empty accounting payload used by new ticks, slab placeholders, and tests.
    pub const DEFAULT: Self = Self {
        liquidity_gross: Liquidity::ZERO,
        liquidity_net: 0,
        fee_growth_outside0_x128: U256::ZERO,
        fee_growth_outside1_x128: U256::ZERO,
    };
}

/// Serializable, owned snapshot of a pool's initialized ticks.
///
/// Unlike a live tick store, this value owns plain serializable data and shares
/// no mutable storage with the source pool. External snapshots are untrusted:
/// rebuild them through the tick-store snapshot loader so spacing alignment,
/// non-zero gross liquidity, and net-liquidity bounds are validated before use.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PoolTicksSnapshot {
    /// Tick spacing used to interpret every key in `inner`.
    pub tick_spacing: TickSpacing,

    /// Initialized tick snapshots keyed by validated protocol tick.
    ///
    /// `BTreeMap` keeps snapshots deterministic and ordered by protocol tick.
    pub inner: BTreeMap<TickIndex, TickInfoInner>,
}
