use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Unbounded},
    sync::Arc,
};

use arb_types::pool::TickEntry;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::v3::{error::SwapSimError, tick_math::MAX_TICK_SPACING, MAX_TICK, MIN_TICK};

pub const MIN_TICK_SPACING: i32 = 1;

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
}

impl From<TickEntry> for TickInfo {
    #[inline]
    fn from(entry: TickEntry) -> Self {
        Self {
            liquidity_gross: entry.liquidity_gross,
            liquidity_net: entry.liquidity_net,
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

/// Ordered initialized tick store for a pool.
///
/// `PoolTicks` is the Rust equivalent of Uniswap's tick mapping plus tick bitmap
/// lookup responsibility. Because this implementation stores only initialized
/// ticks in a `BTreeMap`, the swap loop can locate the next initialized tick via
/// `range()` in `O(log n)` without allocating a sorted vector per swap.
#[derive(Debug, Clone)]
pub struct PoolTicks {
    tick_spacing: i32,
    inner: Arc<RwLock<BTreeMap<i32, TickInfo>>>,
}

impl Serialize for PoolTicks {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let inner_guard = self.inner.read();

        #[derive(Serialize)]
        struct PoolTicksSnapshot<'a> {
            tick_spacing: i32,
            inner: &'a BTreeMap<i32, TickInfo>,
        }

        PoolTicksSnapshot {
            tick_spacing: self.tick_spacing,
            inner: &*inner_guard,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PoolTicks {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct PoolTicksSnapshot {
            tick_spacing: i32,
            inner: BTreeMap<i32, TickInfo>,
        }

        let snapshot = PoolTicksSnapshot::deserialize(deserializer)?;

        Ok(Self {
            tick_spacing: snapshot.tick_spacing,
            inner: Arc::new(RwLock::new(snapshot.inner)),
        })
    }
}

impl PoolTicks {
    /// Create an empty initialized-tick store for one pool.
    pub fn new(tick_spacing: i32) -> Result<Self, SwapSimError> {
        validate_tick_spacing(tick_spacing)?;
        Ok(Self {
            tick_spacing,
            inner: Arc::new(RwLock::new(BTreeMap::new())),
        })
    }

    /// Build a validated tick store from existing [`TickEntry`] values.
    ///
    /// Entries with `liquidity_gross == 0` are rejected because this store should
    /// contain initialized ticks only. If a tick becomes uninitialized, remove it.
    pub fn from_tick_entries(
        entries: impl IntoIterator<Item = TickEntry>,
        tick_spacing: i32,
    ) -> Result<Self, SwapSimError> {
        let ticks = Self::new(tick_spacing)?;
        for entry in entries {
            ticks.insert_entry(entry)?;
        }
        Ok(ticks)
    }

    /// Return the pool tick spacing this tick store was validated against.
    #[inline]
    #[must_use]
    pub fn tick_spacing(&self) -> i32 {
        self.tick_spacing
    }

    /// Insert or replace one initialized tick from an external compatibility type.
    #[inline]
    pub fn insert_entry(&self, entry: TickEntry) -> Result<(), SwapSimError> {
        self.set(entry.tick_idx, TickInfo::from(entry))
    }

    /// Returns a serializable snapshot of all tick data as a map.
    pub fn snapshot(&self) -> BTreeMap<i32, TickInfo> {
        self.inner.read().clone()
    }

    /// Rebuilds a PoolTicks from a snapshot.
    pub fn from_snapshot(
        tick_spacing: i32,
        ticks: BTreeMap<i32, TickInfo>,
    ) -> Result<Self, SwapSimError> {
        let pool_ticks = Self::new(tick_spacing)?;
        for (tick_idx, info) in ticks {
            pool_ticks.inner.write().insert(tick_idx, info);
        }
        Ok(pool_ticks)
    }

    /// Insert, replace, or remove one tick.
    ///
    /// Passing `liquidity_gross == 0` removes the tick. This matches the desired
    /// invariant that the map contains initialized ticks only.
    pub fn set(&self, tick_idx: i32, info: TickInfo) -> Result<(), SwapSimError> {
        validate_tick_index_for_spacing(tick_idx, self.tick_spacing)?;

        let mut ticks = self.inner.write();
        if info.liquidity_gross == 0 {
            ticks.remove(&tick_idx);
        } else {
            ticks.insert(tick_idx, info);
        }
        Ok(())
    }

    /// Remove a tick boundary from the initialized set.
    #[inline]
    pub fn remove(&self, tick_idx: i32) -> Result<Option<TickInfo>, SwapSimError> {
        validate_tick_index_for_spacing(tick_idx, self.tick_spacing)?;
        Ok(self.inner.write().remove(&tick_idx))
    }

    /// Lock the ordered ticks for read-only traversal.
    #[inline]
    pub fn read(&self) -> PoolTicksReadGuard<'_> {
        PoolTicksReadGuard {
            inner: self.inner.read(),
        }
    }

    /// Lock the ordered ticks for mutation.
    #[inline]
    pub fn write(&self) -> PoolTicksWriteGuard<'_> {
        PoolTicksWriteGuard {
            tick_spacing: self.tick_spacing,
            inner: self.inner.write(),
        }
    }
}

/// Read guard exposing Solidity-like tick traversal/crossing operations.
///
/// The swap loop should use this API instead of reaching into the map directly.
pub struct PoolTicksReadGuard<'a> {
    inner: RwLockReadGuard<'a, BTreeMap<i32, TickInfo>>,
}

impl<'a> PoolTicksReadGuard<'a> {
    /// Create a read guard from a snapshot BTreeMap reference.
    pub fn from_snapshot(_ticks: &'a BTreeMap<i32, TickInfo>) -> PoolTicksReadGuard<'a> {
        unimplemented!("use PoolTicksSnapshotReadGuard instead")
    }

    /// Return the next initialized tick boundary in the given swap direction.
    ///   `(MAX_TICK, false)` when none exists.
    #[inline]
    #[must_use]
    pub fn next_initialized_tick(
        &self,
        current_tick: i32,
        zero_for_one: bool,
    ) -> NextInitializedTick {
        if zero_for_one {
            self.inner
                .range(..=current_tick)
                .next_back()
                .map(|(&tick_next, _)| NextInitializedTick {
                    tick_next,
                    initialized: true,
                })
                .unwrap_or(NextInitializedTick {
                    tick_next: MIN_TICK,
                    initialized: false,
                })
        } else {
            self.inner
                .range((Excluded(current_tick), Unbounded))
                .next()
                .map(|(&tick_next, _)| NextInitializedTick {
                    tick_next,
                    initialized: true,
                })
                .unwrap_or(NextInitializedTick {
                    tick_next: MAX_TICK,
                    initialized: false,
                })
        }
    }

    /// Cross an initialized tick and return the direction-adjusted liquidity delta.
    ///
    /// Solidity first calls `Pool.crossTick(...)` to get `liquidityNet`, then
    /// negates it for `zeroForOne`. This method combines those two steps while
    /// keeping the same semantics.
    #[inline]
    pub fn cross_tick(&self, tick_next: i32) -> Result<i128, SwapSimError> {
        let info = self
            .inner
            .get(&tick_next)
            .copied()
            .ok_or(SwapSimError::MissingInitializedTick)?;

        Ok(info.liquidity_net)
    }

    /// Return a copy of a tick if it exists. Useful for tests and diagnostics.
    #[inline]
    #[must_use]
    pub fn get(&self, tick_idx: i32) -> Option<TickInfo> {
        self.inner.get(&tick_idx).copied()
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn first_tick(&self) -> i32 {
        self.inner
            .keys()
            .copied()
            .next()
            .expect("ticks not initialized")
    }

    pub fn last_tick(&self) -> i32 {
        self.inner
            .keys()
            .copied()
            .last()
            .expect("ticks not initialized")
    }
}

/// Write guard for Solidity-like tick updates.
///
/// This owns the equivalent of `Pool.updateTick(...)`: it updates
/// `liquidity_gross`, adjusts `liquidity_net` with the lower/upper tick sign
/// convention, and removes the tick when gross liquidity returns to zero.
pub struct PoolTicksWriteGuard<'a> {
    tick_spacing: i32,
    inner: RwLockWriteGuard<'a, BTreeMap<i32, TickInfo>>,
}

impl<'a> PoolTicksWriteGuard<'a> {
    #[inline]
    fn empty_tick() -> TickInfo {
        TickInfo {
            liquidity_gross: 0,
            liquidity_net: 0,
        }
    }

    fn compute_tick_update(
        before: TickInfo,
        liquidity_delta: i128,
        upper: bool,
    ) -> Result<(TickUpdate, Option<TickInfo>), SwapSimError> {
        let liquidity_gross_after = add_liquidity_delta(before.liquidity_gross, liquidity_delta)?;
        let flipped = (liquidity_gross_after == 0) != (before.liquidity_gross == 0);

        let liquidity_net = if upper {
            before
                .liquidity_net
                .checked_sub(liquidity_delta)
                .ok_or(SwapSimError::LiquidityUnderflow)?
        } else {
            before
                .liquidity_net
                .checked_add(liquidity_delta)
                .ok_or(SwapSimError::LiquidityOverflow)?
        };

        let update = TickUpdate {
            flipped,
            liquidity_gross_after,
        };

        let after = if liquidity_gross_after == 0 {
            None
        } else {
            Some(TickInfo {
                liquidity_gross: liquidity_gross_after,
                liquidity_net,
            })
        };

        Ok((update, after))
    }

    #[inline]
    fn apply_tick_update(&mut self, tick_idx: i32, after: Option<TickInfo>) {
        if let Some(info) = after {
            self.inner.insert(tick_idx, info);
        } else {
            self.inner.remove(&tick_idx);
        }
    }

    /// Update one initialized tick boundary and return whether it flipped.
    ///
    /// `upper = false` mirrors the lower tick update: liquidity net increases
    /// when liquidity is added. `upper = true` mirrors the upper tick update:
    /// liquidity net decreases when liquidity is added.
    pub fn update_tick(
        &mut self,
        tick_idx: i32,
        liquidity_delta: i128,
        upper: bool,
    ) -> Result<TickUpdate, SwapSimError> {
        validate_tick_index_for_spacing(tick_idx, self.tick_spacing)?;

        let before = self
            .inner
            .get(&tick_idx)
            .copied()
            .unwrap_or_else(Self::empty_tick);
        let (update, after) = Self::compute_tick_update(before, liquidity_delta, upper)?;
        self.apply_tick_update(tick_idx, after);
        Ok(update)
    }

    /// Atomically update both lower and upper ticks for a liquidity position.
    ///
    /// Both updates are fully validated before either tick is written, avoiding
    /// partial mutation if the upper tick underflows or max-liquidity checks fail.
    pub fn update_tick_pair(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
        max_liquidity_per_tick: Option<u128>,
    ) -> Result<(TickUpdate, TickUpdate), SwapSimError> {
        validate_tick_index_for_spacing(tick_lower, self.tick_spacing)?;
        validate_tick_index_for_spacing(tick_upper, self.tick_spacing)?;

        let lower_before = self
            .inner
            .get(&tick_lower)
            .copied()
            .unwrap_or_else(Self::empty_tick);
        let upper_before = self
            .inner
            .get(&tick_upper)
            .copied()
            .unwrap_or_else(Self::empty_tick);

        let (lower_update, lower_after) =
            Self::compute_tick_update(lower_before, liquidity_delta, false)?;
        let (upper_update, upper_after) =
            Self::compute_tick_update(upper_before, liquidity_delta, true)?;

        if let Some(max_liquidity_per_tick) = max_liquidity_per_tick {
            if lower_update.liquidity_gross_after > max_liquidity_per_tick
                || upper_update.liquidity_gross_after > max_liquidity_per_tick
            {
                return Err(SwapSimError::LiquidityOverflow);
            }
        }

        self.apply_tick_update(tick_lower, lower_after);
        self.apply_tick_update(tick_upper, upper_after);

        Ok((lower_update, upper_update))
    }

    /// Return a copy of a tick if it exists. Useful for tests and diagnostics.
    #[inline]
    #[must_use]
    pub fn get(&self, tick_idx: i32) -> Option<TickInfo> {
        self.inner.get(&tick_idx).copied()
    }
}

/// Reject tick spacings outside `[MIN_TICK_SPACING, MAX_TICK_SPACING]`.
#[inline]
fn validate_tick_spacing(tick_spacing: i32) -> Result<(), SwapSimError> {
    if tick_spacing < MIN_TICK_SPACING || tick_spacing > MAX_TICK_SPACING {
        Err(SwapSimError::InvalidTickSpacing)
    } else {
        Ok(())
    }
}

/// Reject a single tick index outside `[MIN_TICK, MAX_TICK]`.
///
/// Used to validate both `state.tick` and individual tick-array entries.
#[inline]
fn validate_tick_range(tick: i32) -> Result<(), SwapSimError> {
    if tick < MIN_TICK || tick > MAX_TICK {
        Err(SwapSimError::InvalidTick)
    } else {
        Ok(())
    }
}

/// Reject a tick index outside the absolute range or not aligned to spacing.
#[inline]
fn validate_tick_index_for_spacing(tick: i32, tick_spacing: i32) -> Result<(), SwapSimError> {
    validate_tick_range(tick)?;

    if tick % tick_spacing != 0 {
        Err(SwapSimError::InvalidTick)
    } else {
        Ok(())
    }
}

/// Apply a signed `liquidity_net` delta to the active liquidity.
#[inline]
fn add_liquidity_delta(liquidity: u128, delta: i128) -> Result<u128, SwapSimError> {
    if delta < 0 {
        liquidity
            .checked_sub(delta.unsigned_abs())
            .ok_or(SwapSimError::LiquidityUnderflow)
    } else {
        liquidity
            .checked_add(delta as u128)
            .ok_or(SwapSimError::LiquidityOverflow)
    }
}
