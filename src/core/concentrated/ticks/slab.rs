use std::collections::BTreeMap;

use ruint::aliases::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use slab::Slab;

use crate::{
    Error as SwapSimError,
    core::{
        concentrated::ticks::{bitmap::HierBitmap, slab_value::TickSlabValue},
        types::{
            PoolTicksSnapshot,
            tick::TickIndex,
            tick_slab_indexer::TickSlabIndexer,
            tick_spacing::{TickSpacing, calculate_cache_cap},
            ticks::TickInfoSnapshot,
        },
    },
    v4::{Liquidity, TickInfo},
};

const INITIAL_CACHE_VALUE: u16 = u16::MAX;

/// Sparse, page-based storage for initialized ticks in a concentrated liquidity pool.
///
/// Pool users only interact with initialized ticks when liquidity changes or a swap
/// crosses a boundary. This structure keeps that path fast by grouping 32 nearby
/// tick slots into one page, tracking which pages exist with a bitmap, and preserving
/// neighbor links so swaps can move to the next initialized region without scanning
/// the full protocol tick range. The slab-backed page allocator also smooths out
/// allocation behavior: pools grow by reusable pages instead of paying one separate
/// allocation per initialized tick, reducing memory-allocation spikes during bursts
/// of liquidity updates.
#[derive(Debug)]
pub struct TickSlab {
    /// Tracks which cache pages currently contain at least one initialized tick.
    bitmap: HierBitmap,
    /// Maps a protocol-level page index to the backing `Slab` key.
    ///
    /// `INITIAL_CACHE_VALUE` marks pages that have never been allocated or were
    /// removed after their last initialized tick disappeared.
    cache: Vec<u16>,
    /// Owns reusable tick pages while allowing stable numeric keys.
    values: Slab<TickSlabValue>,
    /// Pool tick spacing used to compress raw protocol ticks into page/slot indexes.
    tick_spacing: TickSpacing,
    /// Protocol cap for gross liquidity at each initialized tick.
    ///
    /// The value is derived from the pool's tick spacing once at construction so
    /// liquidity updates can enforce the Uniswap per-tick limit without
    /// recomputing it on every position change.
    max_liquidity_per_tick: Liquidity,
}

impl Serialize for TickSlab {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.owned_snapshot().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TickSlab {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let snapshot = PoolTicksSnapshot::deserialize(deserializer)?;
        Self::from_snapshot(snapshot.tick_spacing, snapshot.inner).map_err(serde::de::Error::custom)
    }
}

impl TickSlab {
    /// Creates empty tick storage for a pool with the given tick spacing.
    ///
    /// A zero spacing cannot represent a valid pool configuration, so callers get
    /// `None` instead of storage with invalid indexing semantics.
    pub fn new(tick_spacing: TickSpacing) -> Option<Self> {
        let cache_cap = calculate_cache_cap(tick_spacing);

        let max_liquidity_per_tick = tick_spacing.max_liquidity_per_tick();

        Some(Self {
            bitmap: HierBitmap::new(cache_cap),
            cache: vec![INITIAL_CACHE_VALUE; cache_cap as usize],
            values: Slab::new(),
            tick_spacing,
            max_liquidity_per_tick,
        })
    }

    /// Returns the pool tick spacing this slab was indexed against.
    #[inline(always)]
    pub fn tick_spacing(&self) -> TickSpacing {
        self.tick_spacing
    }

    /// Returns the maximum gross liquidity allowed at any one initialized tick.
    ///
    /// Position updates use this cached protocol limit before accepting added
    /// liquidity, preventing one boundary tick from accumulating more liquidity
    /// than the configured spacing permits.
    #[inline(always)]
    pub fn max_liquidity_per_tick(&self) -> Liquidity {
        self.max_liquidity_per_tick
    }

    /// Returns true when no initialized ticks are stored.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Builds the page/slot index used by this slab from a protocol tick.
    ///
    /// The returned indexer is only meaningful for this slab's tick spacing. Use
    /// it as the stable coordinate for reads, writes, removals, and traversal.
    #[inline(always)]
    pub fn indexer(&self, tick_idx: TickIndex) -> TickSlabIndexer {
        TickSlabIndexer::from_tick(tick_idx, self.tick_spacing.as_u32())
    }

    /// Inserts or replaces the liquidity accounting data for an initialized tick.
    ///
    /// The outer page is allocated lazily, keeping memory proportional to active
    /// liquidity regions rather than to the full tick range supported by the pool.
    /// Returns the stored tick payload so callers that create a boundary can keep
    /// using the canonical value owned by the slab. Indexers outside this slab's
    /// spacing-derived cache range are rejected instead of allocating invalid pages.
    pub fn insert(
        &mut self,
        tick_indexer: TickSlabIndexer,
        tick_info: TickInfo,
    ) -> Result<&TickInfo, SwapSimError> {
        let cache_index: u16 = tick_indexer.cache_index();

        if cache_index as usize >= self.cache.len() {
            return Err(SwapSimError::InvalidTick);
        }

        let key = if let Some(key) = self.get_key(cache_index) {
            key
        } else {
            self.insert_empty_slab(cache_index).0
        };

        Ok(self.values[key as usize].insert(&tick_indexer, tick_info))
    }

    /// Returns the liquidity accounting data for an initialized tick, if present.
    pub fn get(&self, tick_indexer: TickSlabIndexer) -> Option<&TickInfo> {
        self.get_slab(tick_indexer.cache_index())
            .and_then(|(_, tick_slab)| tick_slab.get(&tick_indexer))
    }

    /// Returns the next initialized tick after `tick_indexer`, along with its slab indexer.
    ///
    /// Within a page, [`TickSlabValue::next_slot`] uses zero-count instructions on
    /// the page occupancy map to jump to the next set bit strictly after the
    /// current slot. The returned [`TickSlabIndexer`] identifies that initialized
    /// tick, so callers can keep traversing without reconstructing the position
    /// from the `TickInfo`.
    ///
    /// If the current page has no later initialized slot, this follows the
    /// already-maintained page link and returns the first initialized slot in
    /// that next page. When `tick_indexer` points to an absent page, the bitmap
    /// lookup skips directly to the next initialized page.
    pub fn next(&self, tick_indexer: TickSlabIndexer) -> Option<(TickSlabIndexer, &TickInfo)> {
        let cache_index = tick_indexer.cache_index();

        if let Some((_, current_slab)) = self.get_slab(cache_index) {
            return current_slab
                .next_slot(&tick_indexer)
                .map(|(slot_index, tick_info)| {
                    (
                        TickSlabIndexer::from_parts(cache_index, slot_index),
                        tick_info,
                    )
                })
                .or_else(|| {
                    let next_idx = current_slab.next_idx()?;
                    self.get_slab(next_idx)
                        .and_then(|(_, next_slab)| next_slab.first_slot())
                        .map(|(slot_index, tick_info)| {
                            (TickSlabIndexer::from_parts(next_idx, slot_index), tick_info)
                        })
                });
        }

        self.bitmap
            .next_or_eq(cache_index)
            .and_then(|next_idx| self.get_slab(next_idx))
            .and_then(|(next_idx, next_slab)| {
                next_slab.first_slot().map(|(slot_index, tick_info)| {
                    (TickSlabIndexer::from_parts(next_idx, slot_index), tick_info)
                })
            })
    }

    /// Returns the previous initialized tick before `tick_indexer`, along with its slab indexer.
    ///
    /// This mirrors [`next`](Self::next): page-local lookup uses zero counts on
    /// the occupancy bitmap to jump to the previous set bit strictly before the
    /// current slot. If the current page has no earlier initialized slot, the
    /// slab falls back to the previous page link and returns that page's last
    /// initialized slot. When `tick_indexer` points to an absent page, the bitmap
    /// lookup skips directly to the previous initialized page.
    pub fn prev(&self, tick_indexer: TickSlabIndexer) -> Option<(TickSlabIndexer, &TickInfo)> {
        let cache_index = tick_indexer.cache_index();

        if let Some((_, current_slab)) = self.get_slab(cache_index) {
            return current_slab
                .prev_slot(&tick_indexer)
                .map(|(slot_index, tick_info)| {
                    (
                        TickSlabIndexer::from_parts(cache_index, slot_index),
                        tick_info,
                    )
                })
                .or_else(|| {
                    let prev_idx = current_slab.prev_idx()?;
                    self.get_slab(prev_idx)
                        .and_then(|(_, prev_slab)| prev_slab.last_slot())
                        .map(|(slot_index, tick_info)| {
                            (TickSlabIndexer::from_parts(prev_idx, slot_index), tick_info)
                        })
                });
        }

        self.bitmap
            .prev_or_eq(cache_index)
            .and_then(|prev_idx| self.get_slab(prev_idx))
            .and_then(|(prev_idx, prev_slab)| {
                prev_slab.last_slot().map(|(slot_index, tick_info)| {
                    (TickSlabIndexer::from_parts(prev_idx, slot_index), tick_info)
                })
            })
    }

    /// Returns the next initialized tick boundary in the given swap direction.
    ///
    /// `zero_for_one` swaps move left through the tick range, so they use the
    /// previous initialized boundary. The opposite direction moves right and
    /// uses the next initialized boundary. The current slot is never returned as
    /// the next boundary.
    #[inline(always)]
    pub fn next_initialized_tick(
        &self,
        tick_indexer: TickSlabIndexer,
        zero_for_one: bool,
    ) -> Option<(TickSlabIndexer, &TickInfo)> {
        if zero_for_one {
            self.prev(tick_indexer)
        } else {
            self.next(tick_indexer)
        }
    }

    /// Crosses an initialized tick during a swap and returns its liquidity delta.
    ///
    /// Crossing flips each token's fee-growth-outside value around the current
    /// global fee growth, matching the modulo arithmetic used by Uniswap fee
    /// accounting. The stored `liquidity_net` is returned so the caller can apply
    /// the active-liquidity change for the swap direction. Missing boundaries are
    /// treated as state errors because traversal should only land on initialized
    /// ticks.
    #[inline(always)]
    pub fn cross_tick(
        &mut self,
        tick_indexer: TickSlabIndexer,
        fee_growth_global0_x128: U256,
        fee_growth_global1_x128: U256,
    ) -> Result<i128, SwapSimError> {
        self.update_initialized_tick(tick_indexer, |tick_info| {
            tick_info.fee_growth_outside0_x128 =
                fee_growth_global0_x128.wrapping_sub(tick_info.fee_growth_outside0_x128);
            tick_info.fee_growth_outside1_x128 =
                fee_growth_global1_x128.wrapping_sub(tick_info.fee_growth_outside1_x128);

            Ok(tick_info.liquidity_net)
        })
    }

    /// Mutates an initialized tick in place and returns the closure result.
    ///
    /// The update closure only runs when both the page and the selected slot are
    /// initialized. Missing ticks leave storage unchanged and return
    /// `SwapSimError::MissingInitializedTick`, which keeps absent boundaries
    /// distinct from successful updates that intentionally make no field changes.
    /// The closure may return any caller-specific value computed from the updated
    /// tick, such as the tick price or liquidity delta.
    pub fn update_initialized_tick<F, T>(
        &mut self,
        tick_indexer: TickSlabIndexer,
        update: F,
    ) -> Result<T, SwapSimError>
    where
        F: FnOnce(&mut TickInfo) -> Result<T, SwapSimError>,
    {
        self.get_slab_mut(tick_indexer.cache_index())
            .and_then(|(_, slab)| slab.get_mut(&tick_indexer))
            .map_or(Err(SwapSimError::MissingInitializedTick), update)
    }

    /// Removes a tick and frees its page when the page no longer has initialized ticks.
    ///
    /// Empty-page removal keeps long-running pools from accumulating dead pages after
    /// liquidity providers withdraw positions. Neighbor links are rewired before the
    /// page is removed so future boundary traversal still sees a continuous chain.
    pub fn remove(&mut self, tick_indexer: TickSlabIndexer) {
        let cache_index: u16 = tick_indexer.cache_index();

        let empty_slab_links = if let Some((_, target_slab)) = self.get_slab_mut(cache_index) {
            target_slab.remove(&tick_indexer);

            if target_slab.initialized() == 0 {
                Some((target_slab.prev_idx(), target_slab.next_idx()))
            } else {
                None
            }
        } else {
            None
        };

        if let Some((prev_idx, next_idx)) = empty_slab_links {
            if let Some(idx) = next_idx {
                let (_, next_slab) = self.get_slab_mut(idx).expect("next slab should exist");
                next_slab.set_prev_idx(prev_idx);
            }

            if let Some(idx) = prev_idx {
                let (_, prev_slab) = self.get_slab_mut(idx).expect("previous slab should exist");
                prev_slab.set_next_idx(next_idx);
            }

            self.remove_slab(cache_index);
        }
    }

    /// Allocates an empty page at `cache_index` and links it into initialized order.
    #[inline(always)]
    fn insert_empty_slab(&mut self, cache_index: u16) -> (u16, &mut TickSlabValue) {
        self.insert_slab(cache_index, TickSlabValue::new(None, None))
    }

    /// Stores a page and returns its backing allocator key plus mutable value.
    ///
    /// If the page already exists, its payload is replaced in place. Otherwise
    /// the page is inserted into the bitmap, cache lookup table, and linked page
    /// chain as one coherent operation from the caller's perspective.
    fn insert_slab(
        &mut self,
        cache_index: u16,
        mut tick_slab: TickSlabValue,
    ) -> (u16, &mut TickSlabValue) {
        if let Some(key) = self.cache.get(cache_index as usize).cloned()
            && key != INITIAL_CACHE_VALUE
        {
            let slab = self
                .get_slab_mut(cache_index)
                .map(|(_, slab)| slab)
                .expect("slab should exist");

            *slab = tick_slab;
            return (key, slab);
        }

        let prev_idx = self.bitmap.prev(cache_index);
        let next_idx = self.bitmap.next(cache_index);

        if let Some(prev_idx) = prev_idx {
            let (_, prev_slab) = self
                .get_slab_mut(prev_idx)
                .expect("previous slab should exist");
            prev_slab.set_next_idx(Some(cache_index));
        }

        if let Some(next_idx) = next_idx {
            let (_, next_slab) = self.get_slab_mut(next_idx).expect("next slab should exist");
            next_slab.set_prev_idx(Some(cache_index));
        }

        tick_slab.set_prev_idx(prev_idx);
        tick_slab.set_next_idx(next_idx);

        self.bitmap.set(cache_index);

        let key = self.values.insert(tick_slab) as u16;

        *self
            .cache
            .get_mut(cache_index as usize)
            .expect("cache slot should exist") = key;
        (
            key,
            self.values
                .get_mut(key as usize)
                .expect("slab should exist"),
        )
    }

    /// Returns the backing allocator key for an initialized page.
    #[inline(always)]
    fn get_key(&self, cache_index: u16) -> Option<u16> {
        self.cache.get(cache_index as usize).and_then(|key| {
            if *key != INITIAL_CACHE_VALUE {
                Some(*key)
            } else {
                None
            }
        })
    }

    /// Returns a mutable backing allocator key for an initialized page.
    #[inline(always)]
    fn get_key_mut(&mut self, cache_index: u16) -> Option<&mut u16> {
        self.cache.get_mut(cache_index as usize).and_then(|key| {
            if *key != INITIAL_CACHE_VALUE {
                Some(key)
            } else {
                None
            }
        })
    }

    /// Looks up an initialized page by its protocol-level cache index.
    fn get_slab(&self, cache_index: u16) -> Option<(u16, &TickSlabValue)> {
        let key = self.get_key(cache_index)?;

        Some((cache_index, self.values.get(key as usize)?))
    }

    /// Mutably looks up an initialized page by its protocol-level cache index.
    fn get_slab_mut(&mut self, cache_index: u16) -> Option<(u16, &mut TickSlabValue)> {
        let key = self.get_key(cache_index)?;

        Some((cache_index, self.values.get_mut(key as usize)?))
    }

    /// Removes a page from all slab indexes and returns its stored value.
    fn remove_slab(&mut self, cache_index: u16) -> Option<TickSlabValue> {
        let key = {
            let key = self.get_key_mut(cache_index)?;
            let old_key = *key;
            *key = INITIAL_CACHE_VALUE;
            old_key
        };

        self.bitmap.remove(cache_index);

        self.values.try_remove(key as usize)
    }

    /// Returns a serializable snapshot of all initialized tick data.
    ///
    /// Pages are visited in protocol order and each page-local slot is decoded
    /// back into the raw tick for this slab's spacing. Empty cache pages and
    /// uninitialized slots are omitted from the snapshot.
    pub fn snapshot(&self) -> BTreeMap<TickIndex, TickInfoSnapshot> {
        let mut ticks = BTreeMap::new();
        let mut cache_index = self.bitmap.next_or_eq(0);

        while let Some(current_cache_index) = cache_index {
            if let Some((_, slab)) = self.get_slab(current_cache_index) {
                for (slot_index, tick_info) in slab.initialized_slots() {
                    ticks.insert(
                        TickSlabIndexer::from_parts(current_cache_index, slot_index)
                            .to_tick(self.tick_spacing.as_u32()),
                        TickInfoSnapshot::from(*tick_info),
                    );
                }
            }

            cache_index = self.bitmap.next(current_cache_index);
        }

        ticks
    }

    /// Returns an owned snapshot that can be serialized or used to rebuild storage.
    pub fn owned_snapshot(&self) -> PoolTicksSnapshot {
        PoolTicksSnapshot {
            tick_spacing: self.tick_spacing,
            inner: self.snapshot(),
        }
    }

    /// Rebuilds tick storage from a persisted tick snapshot.
    ///
    /// Snapshot entries must be aligned to the supplied spacing and represent
    /// initialized ticks: gross liquidity must be non-zero and net liquidity
    /// must fit within gross liquidity. Invalid snapshots fail before returning
    /// partially trusted storage to callers.
    pub fn from_snapshot<I>(
        tick_spacing: TickSpacing,
        ticks: BTreeMap<TickIndex, I>,
    ) -> Result<Self, SwapSimError>
    where
        I: Into<TickInfoSnapshot>,
    {
        let mut slab = Self::new(tick_spacing).expect("valid TickSpacing must build a TickSlab");

        for (tick_idx, info) in ticks {
            let info = info.into();
            if !tick_idx.for_spacing(tick_spacing) {
                return Err(SwapSimError::InvalidTick);
            }

            if info.liquidity_gross.is_zero()
                || info.liquidity_net.unsigned_abs() > info.liquidity_gross.value()
            {
                return Err(SwapSimError::InvalidTick);
            }

            if info.liquidity_gross > slab.max_liquidity_per_tick {
                return Err(SwapSimError::LiquidityOverflow);
            }

            let indexer = slab.indexer(tick_idx);
            slab.insert(indexer, info.into_tick_info(tick_idx))?;
        }

        Ok(slab)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const TICK_SPACING: TickSpacing = TickSpacing::MIN;

    fn tick(value: i32) -> TickIndex {
        TickIndex::new(value).unwrap()
    }

    fn info(liquidity_gross: u128) -> TickInfo {
        TickInfo {
            liquidity_gross: Liquidity::new(liquidity_gross),
            ..TickInfo::DEFAULT
        }
    }

    fn snapshot_info(liquidity_gross: u128) -> TickInfoSnapshot {
        TickInfoSnapshot::from(info(liquidity_gross))
    }

    fn cache_tick(cache_index: u16) -> TickIndex {
        TickSlabIndexer::from_parts(cache_index, 0).to_tick(TICK_SPACING.as_u32())
    }

    fn assert_page_links(
        slab: &TickSlab,
        cache_index: u16,
        expected_prev: Option<u16>,
        expected_next: Option<u16>,
    ) {
        let (_, page) = slab
            .get_slab(cache_index)
            .unwrap_or_else(|| panic!("expected cache page {cache_index} to exist"));

        assert_eq!(
            page.prev_idx(),
            expected_prev,
            "unexpected prev link for cache page {cache_index}"
        );
        assert_eq!(
            page.next_idx(),
            expected_next,
            "unexpected next link for cache page {cache_index}"
        );
    }

    fn assert_page_absent(slab: &TickSlab, cache_index: u16) {
        assert!(
            slab.get_slab(cache_index).is_none(),
            "expected cache page {cache_index} to be absent"
        );
        assert_eq!(
            slab.cache[cache_index as usize], INITIAL_CACHE_VALUE,
            "removed cache page {cache_index} must not retain an allocator key"
        );
        assert!(
            !slab.bitmap.contains(cache_index),
            "removed cache page {cache_index} must be cleared from the bitmap"
        );
    }

    fn next_result(
        slab: &TickSlab,
        tick_indexer: TickSlabIndexer,
    ) -> Option<(TickSlabIndexer, TickInfo)> {
        slab.next(tick_indexer)
            .map(|(next_indexer, tick_info)| (next_indexer, *tick_info))
    }

    fn prev_result(
        slab: &TickSlab,
        tick_indexer: TickSlabIndexer,
    ) -> Option<(TickSlabIndexer, TickInfo)> {
        slab.prev(tick_indexer)
            .map(|(prev_indexer, tick_info)| (prev_indexer, *tick_info))
    }

    #[test]
    fn cache_capacity_covers_spacing_compressed_protocol_range() {
        let cases = [(1, 60_496), (10, 6_869), (32_767, 3)];

        for (raw_spacing, expected_capacity) in cases {
            let tick_spacing = TickSpacing::new(raw_spacing).unwrap();
            let slab = TickSlab::new(tick_spacing).unwrap();
            assert_eq!(calculate_cache_cap(tick_spacing), expected_capacity);
            assert_eq!(slab.cache.len(), expected_capacity as usize);

            let min_cache_index = slab.indexer(TickIndex::MIN).cache_index();
            let max_cache_index = slab.indexer(TickIndex::MAX).cache_index();

            assert!(
                (min_cache_index as usize) < slab.cache.len(),
                "MIN_TICK cache index must fit for tick_spacing {raw_spacing}"
            );
            assert!(
                (max_cache_index as usize) < slab.cache.len(),
                "MAX_TICK cache index must fit for tick_spacing {raw_spacing}"
            );
        }
    }

    #[test]
    fn zero_tick_spacing_is_rejected_without_building_storage() {
        assert!(TickSpacing::new(0).is_none());
    }

    #[test]
    fn empty_slab_reads_as_uninitialized_and_remove_is_noop() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let indexer = slab.indexer(tick(0));

        assert_eq!(slab.get(indexer), None);

        slab.remove(indexer);

        assert_eq!(slab.get(indexer), None);
        assert!(slab.values.is_empty());
        assert!(slab.cache.iter().all(|key| *key == INITIAL_CACHE_VALUE));
    }

    #[test]
    fn is_empty_tracks_initialized_page_lifecycle() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let first = slab.indexer(tick(0));
        let second = slab.indexer(tick(1));
        assert_eq!(first.cache_index(), second.cache_index());

        assert!(slab.is_empty());

        slab.insert(first, info(100)).unwrap();
        assert!(!slab.is_empty());

        slab.insert(second, info(200)).unwrap();
        assert!(!slab.is_empty());

        slab.remove(first);
        assert!(!slab.is_empty());

        slab.remove(second);
        assert!(slab.is_empty());
    }

    #[test]
    fn insert_and_get_roundtrips_protocol_boundary_ticks() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        let min = slab.indexer(TickIndex::MIN);
        let zero = slab.indexer(tick(0));
        let max = slab.indexer(TickIndex::MAX);

        slab.insert(min, info(1)).unwrap();
        slab.insert(zero, info(2)).unwrap();
        slab.insert(max, info(3)).unwrap();

        assert_eq!(slab.get(min).copied(), Some(info(1)));
        assert_eq!(slab.get(zero).copied(), Some(info(2)));
        assert_eq!(slab.get(max).copied(), Some(info(3)));
    }

    #[test]
    fn tick_spacing_compresses_ticks_into_pool_pages() {
        let mut slab = TickSlab::new(TickSpacing::new(10).unwrap()).unwrap();
        let compressed_tick = slab.indexer(tick(160));
        let neighboring_tick = slab.indexer(tick(150));

        slab.insert(compressed_tick, info(42)).unwrap();

        assert_eq!(slab.get(compressed_tick).copied(), Some(info(42)));
        assert_eq!(slab.get(neighboring_tick), None);
        assert_ne!(compressed_tick, neighboring_tick);
    }

    #[test]
    fn unaligned_ticks_follow_the_same_spacing_compression_as_indexer() {
        let mut slab = TickSlab::new(TickSpacing::new(10).unwrap()).unwrap();
        let aligned_tick = slab.indexer(tick(10));
        let unaligned_tick_in_same_bucket = slab.indexer(tick(19));

        slab.insert(aligned_tick, info(100)).unwrap();

        assert_eq!(aligned_tick, unaligned_tick_in_same_bucket);
        assert_eq!(
            slab.get(unaligned_tick_in_same_bucket).copied(),
            Some(info(100))
        );
    }

    #[test]
    fn inserting_default_tick_info_still_marks_tick_as_present() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let indexer = slab.indexer(tick(0));

        slab.insert(indexer, TickInfo::DEFAULT).unwrap();

        assert_eq!(slab.get(indexer).copied(), Some(TickInfo::DEFAULT));
        assert_eq!(slab.values.len(), 1);
    }

    #[test]
    fn insert_rejects_indexer_outside_slab_cache_range() {
        let source = TickSlab::new(TICK_SPACING).unwrap();
        let mut target = TickSlab::new(TickSpacing::MAX).unwrap();
        let foreign_indexer = source.indexer(TickIndex::MAX);

        assert_eq!(
            target.insert(foreign_indexer, info(100)).err(),
            Some(SwapSimError::InvalidTick)
        );
        assert!(target.is_empty());
    }

    #[test]
    fn replacing_existing_tick_updates_value_without_allocating_another_page() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let indexer = slab.indexer(tick(7));

        slab.insert(indexer, info(100)).unwrap();
        slab.insert(indexer, info(250)).unwrap();

        assert_eq!(slab.get(indexer).copied(), Some(info(250)));
        assert_eq!(slab.values.len(), 1);
        assert_eq!(
            slab.get_slab(indexer.cache_index())
                .expect("page must exist")
                .1
                .initialized(),
            1
        );
    }

    #[test]
    fn update_initialized_tick_mutates_existing_tick() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let indexer = slab.indexer(tick(7));

        slab.insert(indexer, info(100)).unwrap();

        let updated = slab.update_initialized_tick(indexer, |tick_info| {
            tick_info.liquidity_gross = Liquidity::new(250);
            Ok(true)
        });

        assert_eq!(updated, Ok(true));
        assert_eq!(slab.get(indexer).copied(), Some(info(250)));
    }

    #[test]
    fn update_initialized_tick_returns_error_for_absent_tick() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let indexer = slab.indexer(tick(7));
        let mut called = false;

        let updated = slab.update_initialized_tick(indexer, |_| {
            called = true;
            Ok(true)
        });

        assert_eq!(updated, Err(SwapSimError::MissingInitializedTick));
        assert!(!called);
        assert_eq!(slab.get(indexer), None);
    }

    #[test]
    fn update_initialized_tick_returns_error_for_missing_slot_inside_existing_page() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let initialized = slab.indexer(tick(0));
        let missing = slab.indexer(tick(1));
        let mut called = false;
        assert_eq!(initialized.cache_index(), missing.cache_index());
        assert_ne!(initialized, missing);

        slab.insert(initialized, info(100)).unwrap();

        let updated = slab.update_initialized_tick(missing, |_| {
            called = true;
            Ok(true)
        });

        assert_eq!(updated, Err(SwapSimError::MissingInitializedTick));
        assert!(!called);
        assert_eq!(slab.get(initialized).copied(), Some(info(100)));
        assert_eq!(slab.get(missing), None);
        assert_eq!(
            slab.get_slab(initialized.cache_index())
                .expect("page must exist")
                .1
                .initialized(),
            1
        );
    }

    #[test]
    fn removing_one_tick_keeps_other_ticks_in_the_same_page() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let first = slab.indexer(tick(0));
        let second = slab.indexer(tick(1));
        assert_eq!(first.cache_index(), second.cache_index());

        slab.insert(first, info(100)).unwrap();
        slab.insert(second, info(200)).unwrap();
        slab.remove(first);

        assert_eq!(slab.get(first), None);
        assert_eq!(slab.get(second).copied(), Some(info(200)));
        assert_eq!(slab.values.len(), 1);
        assert_eq!(
            slab.get_slab(first.cache_index())
                .expect("page must stay allocated")
                .1
                .initialized(),
            1
        );
    }

    #[test]
    fn removing_missing_slot_inside_existing_page_is_noop() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let initialized = slab.indexer(tick(0));
        let missing = slab.indexer(tick(1));
        assert_eq!(initialized.cache_index(), missing.cache_index());
        assert_ne!(initialized, missing);

        slab.insert(initialized, info(100)).unwrap();
        slab.remove(missing);

        assert_eq!(slab.get(initialized).copied(), Some(info(100)));
        assert_eq!(slab.get(missing), None);
        assert_eq!(slab.values.len(), 1);
        assert_eq!(
            slab.get_slab(initialized.cache_index())
                .expect("page must stay allocated")
                .1
                .initialized(),
            1
        );
    }

    #[test]
    fn removing_last_tick_frees_page_from_cache_bitmap_and_storage() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let indexer = slab.indexer(tick(16));
        let cache_index = indexer.cache_index();

        slab.insert(indexer, info(100)).unwrap();
        slab.remove(indexer);

        assert_eq!(slab.get(indexer), None);
        assert!(slab.values.is_empty());
        assert_page_absent(&slab, cache_index);
    }

    #[test]
    fn page_can_be_reinserted_after_being_freed() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let indexer = slab.indexer(tick(16));
        let cache_index = indexer.cache_index();

        slab.insert(indexer, info(100)).unwrap();
        slab.remove(indexer);
        slab.insert(indexer, info(300)).unwrap();

        assert_eq!(slab.get(indexer).copied(), Some(info(300)));
        assert_eq!(slab.values.len(), 1);
        assert!(slab.bitmap.contains(cache_index));
        assert_ne!(slab.cache[cache_index as usize], INITIAL_CACHE_VALUE);
    }

    #[test]
    fn pages_are_linked_in_cache_order_regardless_of_insert_order() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        for cache_index in [32_771, 32_767, 32_769] {
            let indexer = slab.indexer(cache_tick(cache_index));
            slab.insert(indexer, info(cache_index as u128)).unwrap();
        }

        assert_page_links(&slab, 32_767, None, Some(32_769));
        assert_page_links(&slab, 32_769, Some(32_767), Some(32_771));
        assert_page_links(&slab, 32_771, Some(32_769), None);
    }

    #[test]
    fn removing_middle_page_rewires_neighbor_pages() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        for cache_index in [32_767, 32_769, 32_771] {
            let indexer = slab.indexer(cache_tick(cache_index));
            slab.insert(indexer, info(cache_index as u128)).unwrap();
        }

        let middle = slab.indexer(cache_tick(32_769));
        slab.remove(middle);

        assert_page_absent(&slab, 32_769);
        assert_page_links(&slab, 32_767, None, Some(32_771));
        assert_page_links(&slab, 32_771, Some(32_767), None);
    }

    #[test]
    fn removing_head_and_tail_pages_preserves_remaining_chain() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        for cache_index in [32_767, 32_769, 32_771] {
            let indexer = slab.indexer(cache_tick(cache_index));
            slab.insert(indexer, info(cache_index as u128)).unwrap();
        }

        slab.remove(slab.indexer(cache_tick(32_767)));
        assert_page_absent(&slab, 32_767);
        assert_page_links(&slab, 32_769, None, Some(32_771));
        assert_page_links(&slab, 32_771, Some(32_769), None);

        slab.remove(slab.indexer(cache_tick(32_771)));
        assert_page_absent(&slab, 32_771);
        assert_page_links(&slab, 32_769, None, None);
    }

    #[test]
    fn next_and_prev_find_neighbors_inside_the_same_page() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        let slot_0 = slab.indexer(tick(0));
        let slot_1 = slab.indexer(tick(1));
        let slot_4 = slab.indexer(tick(2));

        slab.insert(slot_0, info(10)).unwrap();
        slab.insert(slot_1, info(20)).unwrap();
        slab.insert(slot_4, info(40)).unwrap();

        assert_eq!(next_result(&slab, slot_0), Some((slot_1, info(20))));
        assert_eq!(next_result(&slab, slot_1), Some((slot_4, info(40))));
        assert_eq!(prev_result(&slab, slot_4), Some((slot_1, info(20))));
        assert_eq!(prev_result(&slab, slot_1), Some((slot_0, info(10))));
    }

    #[test]
    fn next_and_prev_cross_page_links() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        let page_0_last = slab.indexer(tick(-16));
        let page_1_first = slab.indexer(tick(16));
        let page_3_first = slab.indexer(tick(48));

        slab.insert(page_0_last, info(31)).unwrap();
        slab.insert(page_1_first, info(100)).unwrap();
        slab.insert(page_3_first, info(300)).unwrap();

        assert_eq!(
            next_result(&slab, page_0_last),
            Some((page_1_first, info(100)))
        );
        assert_eq!(
            next_result(&slab, page_1_first),
            Some((page_3_first, info(300)))
        );
        assert_eq!(slab.next(page_3_first), None);

        assert_eq!(
            prev_result(&slab, page_3_first),
            Some((page_1_first, info(100)))
        );
        assert_eq!(
            prev_result(&slab, page_1_first),
            Some((page_0_last, info(31)))
        );
        assert_eq!(slab.prev(page_0_last), None);
    }

    #[test]
    fn next_initialized_tick_returns_protocol_order_boundary_for_swap_direction() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        let page_0_last = slab.indexer(tick(-16));
        let page_1_first = slab.indexer(tick(16));
        let page_3_first = slab.indexer(tick(48));

        slab.insert(page_0_last, info(31)).unwrap();
        slab.insert(page_1_first, info(100)).unwrap();
        slab.insert(page_3_first, info(300)).unwrap();

        assert_eq!(
            slab.next_initialized_tick(page_1_first, true)
                .map(|(indexer, tick_info)| (indexer, *tick_info)),
            Some((page_0_last, info(31)))
        );
        assert_eq!(
            slab.next_initialized_tick(page_1_first, false)
                .map(|(indexer, tick_info)| (indexer, *tick_info)),
            Some((page_3_first, info(300)))
        );
    }

    #[test]
    fn next_and_prev_skip_from_absent_pages() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        let page_1_first = slab.indexer(tick(16));
        let absent_page_2_first = slab.indexer(tick(32));
        let page_3_first = slab.indexer(tick(48));

        slab.insert(page_1_first, info(100)).unwrap();
        slab.insert(page_3_first, info(300)).unwrap();

        assert_eq!(
            next_result(&slab, absent_page_2_first),
            Some((page_3_first, info(300)))
        );
        assert_eq!(
            prev_result(&slab, absent_page_2_first),
            Some((page_1_first, info(100)))
        );
    }

    #[test]
    fn sparse_page_storage_matches_reference_model_for_mixed_operations() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let mut model = BTreeMap::new();

        let operations = [
            (8, Some(info(80))),
            (-9, Some(info(90))),
            (64, Some(info(640))),
            (8, Some(info(81))),
            (512, Some(info(5120))),
            (-9, None),
            (0, Some(TickInfo::DEFAULT)),
            (64, None),
            (*TickIndex::MIN, Some(info(1))),
            (*TickIndex::MAX, Some(info(2))),
        ];

        for (raw_tick, maybe_info) in operations {
            let indexer = slab.indexer(tick(raw_tick));

            match maybe_info {
                Some(tick_info) => {
                    slab.insert(indexer, tick_info).unwrap();
                    model.insert(raw_tick, tick_info);
                }
                None => {
                    slab.remove(indexer);
                    model.remove(&raw_tick);
                }
            }

            for probe in [*TickIndex::MIN, -9, 0, 8, 64, 512, *TickIndex::MAX] {
                let actual = slab.get(slab.indexer(tick(probe))).copied();
                let expected = model.get(&probe).copied();
                assert_eq!(
                    actual, expected,
                    "slab diverged from reference model after operation on tick {raw_tick}, probing tick {probe}"
                );
            }
        }
    }

    #[test]
    fn snapshot_decodes_negative_and_positive_ticks_for_tick_spacing() {
        let tick_spacing = TickSpacing::new(10).unwrap();
        let mut slab = TickSlab::new(tick_spacing).unwrap();
        let ticks = BTreeMap::from([
            (tick(-40), snapshot_info(400)),
            (tick(-10), snapshot_info(100)),
            (tick(0), snapshot_info(1)),
            (tick(30), snapshot_info(300)),
        ]);

        for (&tick_idx, &tick_info) in &ticks {
            slab.insert(slab.indexer(tick_idx), tick_info.into_tick_info(tick_idx))
                .unwrap();
        }

        assert_eq!(slab.snapshot(), ticks);
    }

    #[test]
    fn owned_snapshot_returns_independent_tick_map() {
        let tick_spacing = TickSpacing::new(10).unwrap();
        let mut slab = TickSlab::new(tick_spacing).unwrap();
        let lower = tick(-20);
        let upper = tick(30);

        slab.insert(slab.indexer(lower), info(200)).unwrap();
        slab.insert(slab.indexer(upper), info(300)).unwrap();

        let snapshot = slab.owned_snapshot();

        assert_eq!(snapshot.tick_spacing, tick_spacing);
        assert_eq!(
            snapshot.inner.get(&lower).copied(),
            Some(snapshot_info(200))
        );
        assert_eq!(
            snapshot.inner.get(&upper).copied(),
            Some(snapshot_info(300))
        );

        slab.remove(slab.indexer(lower));

        assert_eq!(
            snapshot.inner.get(&lower).copied(),
            Some(snapshot_info(200))
        );
        assert_eq!(slab.get(slab.indexer(lower)), None);
    }

    #[test]
    fn from_snapshot_rebuilds_slab_storage() {
        let tick_spacing = TickSpacing::new(10).unwrap();
        let ticks = BTreeMap::from([
            (tick(-20), snapshot_info(200)),
            (tick(30), snapshot_info(300)),
        ]);

        let slab = TickSlab::from_snapshot(tick_spacing, ticks.clone()).unwrap();

        assert_eq!(slab.tick_spacing(), tick_spacing);
        assert_eq!(slab.snapshot(), ticks);
    }

    #[test]
    fn from_snapshot_rejects_unaligned_ticks() {
        let tick_spacing = TickSpacing::new(10).unwrap();
        let ticks = BTreeMap::from([(tick(11), snapshot_info(100))]);

        assert_eq!(
            TickSlab::from_snapshot(tick_spacing, ticks).err(),
            Some(SwapSimError::InvalidTick)
        );
    }

    #[test]
    fn from_snapshot_rejects_liquidity_net_above_gross() {
        let ticks = BTreeMap::from([(
            tick(10),
            TickInfoSnapshot {
                liquidity_gross: Liquidity::new(100),
                liquidity_net: 101,
                ..TickInfoSnapshot::DEFAULT
            },
        )]);

        assert_eq!(
            TickSlab::from_snapshot(TICK_SPACING, ticks).err(),
            Some(SwapSimError::InvalidTick)
        );
    }

    #[test]
    fn from_snapshot_rejects_liquidity_gross_above_tick_cap() {
        let ticks = BTreeMap::from([(
            tick(10),
            TickInfoSnapshot {
                liquidity_gross: Liquidity::MAX,
                liquidity_net: 0,
                ..TickInfoSnapshot::DEFAULT
            },
        )]);

        assert_eq!(
            TickSlab::from_snapshot(TICK_SPACING, ticks).err(),
            Some(SwapSimError::LiquidityOverflow)
        );
    }

    #[test]
    fn from_snapshot_rejects_uninitialized_tick_entries() {
        let ticks = BTreeMap::from([(tick(10), TickInfoSnapshot::DEFAULT)]);

        assert_eq!(
            TickSlab::from_snapshot(TICK_SPACING, ticks).err(),
            Some(SwapSimError::InvalidTick)
        );
    }

    #[test]
    fn serde_roundtrips_through_pool_ticks_snapshot() {
        let tick_spacing = TickSpacing::new(10).unwrap();
        let mut slab = TickSlab::new(tick_spacing).unwrap();
        let ticks = BTreeMap::from([
            (tick(-20), snapshot_info(200)),
            (tick(30), snapshot_info(300)),
        ]);

        for (&tick_idx, &tick_info) in &ticks {
            slab.insert(slab.indexer(tick_idx), tick_info.into_tick_info(tick_idx))
                .unwrap();
        }

        let json = serde_json::to_string(&slab).unwrap();
        let decoded: TickSlab = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.tick_spacing(), tick_spacing);
        assert_eq!(decoded.snapshot(), ticks);
    }

    #[test]
    fn serde_rejects_invalid_pool_ticks_snapshot() {
        let tick_spacing = TickSpacing::new(10).unwrap();
        let snapshot = PoolTicksSnapshot {
            tick_spacing,
            inner: BTreeMap::from([(tick(11), snapshot_info(100))]),
        };
        let json = serde_json::to_string(&snapshot).unwrap();

        assert!(serde_json::from_str::<TickSlab>(&json).is_err());
    }
}
