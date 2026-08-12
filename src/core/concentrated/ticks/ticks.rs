use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Unbounded},
    sync::Arc,
};

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use ruint::aliases::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    Error as SwapSimError,
    core::{
        math::tick::MAX_TICK_SPACING,
        types::{NextInitializedTick, PoolTicksSnapshot, TickInfo, TickUpdate, tick::TickIndex},
    },
};

pub const MIN_TICK_SPACING: u32 = 1;

/// Ordered initialized tick store for a pool.
///
/// `PoolTicks` is the Rust equivalent of Uniswap's tick mapping plus tick bitmap
/// lookup responsibility. Because this implementation stores only initialized
/// ticks in a `BTreeMap`, the swap loop can locate the next initialized tick via
/// `range()` in `O(log n)` without allocating a sorted vector per swap.
#[derive(Debug, Clone)]
pub struct PoolTicks {
    tick_spacing: u32,
    inner: Arc<RwLock<BTreeMap<TickIndex, TickInfo>>>,
}

impl Serialize for PoolTicks {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.owned_snapshot().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PoolTicks {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let snapshot = PoolTicksSnapshot::deserialize(deserializer)?;
        Self::from_snapshot(snapshot.tick_spacing, snapshot.inner).map_err(serde::de::Error::custom)
    }
}

impl PoolTicks {
    /// Create an empty initialized-tick store for one pool.
    pub fn new(tick_spacing: u32) -> Result<Self, SwapSimError> {
        validate_tick_spacing(tick_spacing)?;
        Ok(Self {
            tick_spacing,
            inner: Arc::new(RwLock::new(BTreeMap::new())),
        })
    }

    /// Return the pool tick spacing this tick store was validated against.
    #[inline]
    #[must_use]
    pub fn tick_spacing(&self) -> u32 {
        self.tick_spacing
    }

    /// Returns a serializable snapshot of all tick data as a map.
    pub fn snapshot(&self) -> BTreeMap<TickIndex, TickInfo> {
        self.inner.read().clone()
    }

    /// Returns an owned snapshot that can be serialized or used to create an
    /// independent pool.
    pub fn owned_snapshot(&self) -> PoolTicksSnapshot {
        PoolTicksSnapshot {
            tick_spacing: self.tick_spacing,
            inner: self.snapshot(),
        }
    }

    /// Rebuilds a PoolTicks from a snapshot.
    pub fn from_snapshot(
        tick_spacing: u32,
        ticks: BTreeMap<TickIndex, TickInfo>,
    ) -> Result<Self, SwapSimError> {
        let pool_ticks = Self::new(tick_spacing)?;
        for (tick_idx, info) in ticks {
            if info.liquidity_gross == 0 || info.liquidity_net.unsigned_abs() > info.liquidity_gross
            {
                return Err(SwapSimError::InvalidTick);
            }
            pool_ticks.set(tick_idx, info)?;
        }
        Ok(pool_ticks)
    }

    /// Insert, replace, or remove one tick.
    ///
    /// Passing `liquidity_gross == 0` removes the tick. This matches the desired
    /// invariant that the map contains initialized ticks only.
    pub fn set(&self, tick_idx: TickIndex, info: TickInfo) -> Result<(), SwapSimError> {
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
    pub fn remove(&self, tick_idx: TickIndex) -> Result<Option<TickInfo>, SwapSimError> {
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
    inner: RwLockReadGuard<'a, BTreeMap<TickIndex, TickInfo>>,
}

impl<'a> PoolTicksReadGuard<'a> {
    /// Returns an owned copy of the initialized ticks while retaining the read
    /// lock for the caller's surrounding state snapshot.
    pub fn snapshot(&self) -> BTreeMap<TickIndex, TickInfo> {
        self.inner.clone()
    }

    /// Return the next initialized tick boundary in the given swap direction.
    ///   `(MAX_TICK, false)` when none exists.
    #[inline]
    #[must_use]
    pub fn next_initialized_tick(
        &self,
        current_tick: TickIndex,
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
                    tick_next: TickIndex::MIN,
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
                    tick_next: TickIndex::MAX,
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
    pub fn cross_tick(&self, tick_next: TickIndex) -> Result<i128, SwapSimError> {
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
    pub fn get(&self, tick_idx: TickIndex) -> Option<TickInfo> {
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

    pub fn first_tick(&self) -> TickIndex {
        self.inner
            .keys()
            .copied()
            .next()
            .expect("ticks not initialized")
    }

    pub fn last_tick(&self) -> TickIndex {
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
    tick_spacing: u32,
    inner: RwLockWriteGuard<'a, BTreeMap<TickIndex, TickInfo>>,
}

impl<'a> PoolTicksWriteGuard<'a> {
    #[inline]
    pub fn next_initialized_tick(
        &self,
        current_tick: TickIndex,
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
                    tick_next: TickIndex::MIN,
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
                    tick_next: TickIndex::MAX,
                    initialized: false,
                })
        }
    }

    #[inline]
    pub fn cross_tick(&self, tick_next: TickIndex) -> Result<i128, SwapSimError> {
        self.inner
            .get(&tick_next)
            .map(|info| info.liquidity_net)
            .ok_or(SwapSimError::MissingInitializedTick)
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
            Some(TickInfo::new(
                before.sqrt_price_x96,
                liquidity_gross_after,
                liquidity_net,
                before.fee_growth_outside0_x128,
                before.fee_growth_outside1_x128,
            ))
        };

        Ok((update, after))
    }

    #[inline]
    fn apply_tick_update(&mut self, tick_idx: TickIndex, after: Option<TickInfo>) {
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
        tick_idx: TickIndex,
        liquidity_delta: i128,
        upper: bool,
    ) -> Result<TickUpdate, SwapSimError> {
        validate_tick_index_for_spacing(tick_idx, self.tick_spacing)?;

        let before = self
            .inner
            .get(&tick_idx)
            .copied()
            .unwrap_or_else(TickInfo::default);
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
        tick_lower: TickIndex,
        tick_upper: TickIndex,
        liquidity_delta: i128,
        max_liquidity_per_tick: Option<u128>,
    ) -> Result<(TickUpdate, TickUpdate), SwapSimError> {
        validate_tick_index_for_spacing(tick_lower, self.tick_spacing)?;
        validate_tick_index_for_spacing(tick_upper, self.tick_spacing)?;

        let lower_before = self
            .inner
            .get(&tick_lower)
            .copied()
            .unwrap_or_else(|| TickInfo::DEFAULT);
        let upper_before = self
            .inner
            .get(&tick_upper)
            .copied()
            .unwrap_or_else(|| TickInfo::DEFAULT);

        let (lower_update, lower_after) =
            Self::compute_tick_update(lower_before, liquidity_delta, false)?;
        let (upper_update, upper_after) =
            Self::compute_tick_update(upper_before, liquidity_delta, true)?;

        if let Some(max_liquidity_per_tick) = max_liquidity_per_tick
            && (lower_update.liquidity_gross_after > max_liquidity_per_tick
                || upper_update.liquidity_gross_after > max_liquidity_per_tick)
        {
            return Err(SwapSimError::LiquidityOverflow);
        }

        self.apply_tick_update(tick_lower, lower_after);
        self.apply_tick_update(tick_upper, upper_after);

        Ok((lower_update, upper_update))
    }

    /// Initialize a newly-created tick's fee-growth outside values.
    ///
    /// Uniswap treats all fee growth before initialization as having happened
    /// below the tick. Therefore ticks at or below the current tick snapshot
    /// the current global fee growth.
    pub fn initialize_fee_growth_if_needed(
        &mut self,
        tick_idx: TickIndex,
        was_uninitialized: bool,
        current_tick: TickIndex,
        fee_growth_global0_x128: U256,
        fee_growth_global1_x128: U256,
    ) {
        if was_uninitialized
            && tick_idx <= current_tick
            && let Some(info) = self.inner.get_mut(&tick_idx)
        {
            info.fee_growth_outside0_x128 = fee_growth_global0_x128;
            info.fee_growth_outside1_x128 = fee_growth_global1_x128;
        }
    }

    /// Apply Uniswap's fee-growth transition when price crosses a tick.
    pub fn cross_fee_growth(
        &mut self,
        tick_idx: TickIndex,
        fee_growth_global0_x128: U256,
        fee_growth_global1_x128: U256,
    ) -> Result<(), SwapSimError> {
        let info = self
            .inner
            .get_mut(&tick_idx)
            .ok_or(SwapSimError::MissingInitializedTick)?;
        info.fee_growth_outside0_x128 =
            fee_growth_global0_x128.wrapping_sub(info.fee_growth_outside0_x128);
        info.fee_growth_outside1_x128 =
            fee_growth_global1_x128.wrapping_sub(info.fee_growth_outside1_x128);
        Ok(())
    }

    /// Return a copy of a tick if it exists. Useful for tests and diagnostics.
    #[inline]
    #[must_use]
    pub fn get(&self, tick_idx: TickIndex) -> Option<TickInfo> {
        self.inner.get(&tick_idx).copied()
    }
}

/// Reject tick spacings outside `[MIN_TICK_SPACING, MAX_TICK_SPACING]`.
#[inline]
fn validate_tick_spacing(tick_spacing: u32) -> Result<(), SwapSimError> {
    if !(MIN_TICK_SPACING..=MAX_TICK_SPACING).contains(&tick_spacing) {
        Err(SwapSimError::InvalidTickSpacing)
    } else {
        Ok(())
    }
}

// TODO: remove method
/// Reject a tick index outside the absolute range or not aligned to spacing.
#[inline]
fn validate_tick_index_for_spacing(tick: TickIndex, tick_spacing: u32) -> Result<(), SwapSimError> {
    if tick.for_spacing(tick_spacing) {
        Ok(())
    } else {
        Err(SwapSimError::InvalidTick)
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
