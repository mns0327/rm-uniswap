use slab::Slab;

use crate::{
    core::{
        concentrated::ticks::{bitmap::HierBitmap, slab_value::TickSlabValue},
        types::{tick::TickIndex, tick_slab_indexer::TickSlabIndexer},
    },
    v4::{
        TickInfo,
        tick_math::{MAX_TICK, MIN_TICK},
    },
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
pub(crate) struct TickSlab {
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
    tick_spacing: u32,
}

impl TickSlab {
    /// Creates empty tick storage for a pool with the given tick spacing.
    ///
    /// A zero spacing cannot represent a valid pool configuration, so callers get
    /// `None` instead of storage with invalid indexing semantics.
    pub fn new(tick_spacing: u32) -> Option<Self> {
        if tick_spacing == 0 {
            return None;
        }

        let cache_cap = calculate_cache_cap(tick_spacing);

        Some(Self {
            bitmap: HierBitmap::new(cache_cap as u16),
            cache: vec![INITIAL_CACHE_VALUE; cache_cap as usize],
            values: Slab::new(),
            tick_spacing,
        })
    }

    #[inline(always)]
    /// Builds the page/slot index used by this slab from a protocol tick.
    pub fn indexer(&self, tick_idx: TickIndex) -> TickSlabIndexer {
        TickSlabIndexer::from_tick(tick_idx, self.tick_spacing)
    }

    /// Inserts or replaces the liquidity accounting data for an initialized tick.
    ///
    /// The outer page is allocated lazily, keeping memory proportional to active
    /// liquidity regions rather than to the full tick range supported by the pool.
    pub fn insert(&mut self, tick_indexer: TickSlabIndexer, tick_info: TickInfo) {
        let cache_index: u16 = tick_indexer.cache_index();

        let slab = if let Some((_, slab)) = self.get_slab_mut(cache_index) {
            slab
        } else {
            self.insert_empty_slab(cache_index)
        };

        slab.insert(&tick_indexer, tick_info);
    }

    /// Returns the liquidity accounting data for an initialized tick, if present.
    pub fn get(&self, tick_indexer: TickSlabIndexer) -> Option<&TickInfo> {
        self.get_slab(tick_indexer.cache_index())
            .and_then(|(_, tick_slab)| tick_slab.get(&tick_indexer))
    }

    /// Returns the next initialized tick after `tick_indexer`.
    ///
    /// Within a page, [`TickSlabValue::next`] uses zero-count instructions on
    /// the page occupancy map to jump to the next set bit. If the current page
    /// has no later initialized slot, this follows the already-maintained page
    /// link and returns the first initialized slot in that next page.
    pub fn next(&self, tick_indexer: TickSlabIndexer) -> Option<&TickInfo> {
        let cache_index = tick_indexer.cache_index();

        if let Some((_, current_slab)) = self.get_slab(cache_index) {
            return current_slab.next(&tick_indexer).or_else(|| {
                let next_idx = current_slab.next_idx()?;
                self.get_slab(next_idx)
                    .and_then(|(_, next_slab)| next_slab.first())
            });
        }

        self.bitmap
            .next_or_eq(cache_index)
            .and_then(|next_idx| self.get_slab(next_idx))
            .and_then(|(_, next_slab)| next_slab.first())
    }

    /// Returns the previous initialized tick before `tick_indexer`.
    ///
    /// This mirrors [`next`](Self::next): page-local lookup uses zero counts on
    /// the occupancy bitmap, then the slab falls back to the previous page link
    /// and returns that page's last initialized slot.
    pub fn prev(&self, tick_indexer: TickSlabIndexer) -> Option<&TickInfo> {
        let cache_index = tick_indexer.cache_index();

        if let Some((_, current_slab)) = self.get_slab(cache_index) {
            return current_slab.prev(&tick_indexer).or_else(|| {
                let prev_idx = current_slab.prev_idx()?;
                self.get_slab(prev_idx)
                    .and_then(|(_, prev_slab)| prev_slab.last())
            });
        }

        self.bitmap
            .prev_or_eq(cache_index)
            .and_then(|prev_idx| self.get_slab(prev_idx))
            .and_then(|(_, prev_slab)| prev_slab.last())
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
    fn insert_empty_slab(&mut self, cache_index: u16) -> &mut TickSlabValue {
        self.insert_slab(cache_index, TickSlabValue::new(None, None))
    }

    /// Stores a page and updates bitmap/cache/neighbor metadata atomically from the
    /// caller's perspective.
    fn insert_slab(
        &mut self,
        cache_index: u16,
        mut tick_slab: TickSlabValue,
    ) -> &mut TickSlabValue {
        if self
            .cache
            .get(cache_index as usize)
            .is_some_and(|key| *key != INITIAL_CACHE_VALUE)
        {
            let slab = self
                .get_slab_mut(cache_index)
                .map(|(_, slab)| slab)
                .expect("slab should exist");

            *slab = tick_slab;
            return slab;
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
        self.values
            .get_mut(key as usize)
            .expect("slab should exist")
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
}

/// Calculates the number of page buckets required to cover the protocol tick range.
#[inline(always)]
fn calculate_cache_cap(tick_spacing: u32) -> u16 {
    (((MAX_TICK - MIN_TICK) as usize / tick_spacing as usize) >> 5) as u16 + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const TICK_SPACING: u32 = 1;

    fn tick(value: i32) -> TickIndex {
        TickIndex::new(value).unwrap()
    }

    fn info(liquidity_gross: u128) -> TickInfo {
        TickInfo {
            liquidity_gross,
            ..TickInfo::DEFAULT
        }
    }

    fn cache_tick(cache_index: u16) -> TickIndex {
        tick(cache_index as i32 * 16)
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

    #[test]
    fn cache_capacity_covers_spacing_compressed_protocol_range() {
        let cases = [(1, 55_455), (10, 5_546), (32_767, 2)];

        for (tick_spacing, expected_capacity) in cases {
            let slab = TickSlab::new(tick_spacing).unwrap();
            assert_eq!(calculate_cache_cap(tick_spacing), expected_capacity);
            assert_eq!(slab.cache.len(), expected_capacity as usize);

            let min_cache_index = slab.indexer(TickIndex::MIN).cache_index();
            let max_cache_index = slab.indexer(TickIndex::MAX).cache_index();

            assert!(
                (min_cache_index as usize) < slab.cache.len(),
                "MIN_TICK cache index must fit for tick_spacing {tick_spacing}"
            );
            assert!(
                (max_cache_index as usize) < slab.cache.len(),
                "MAX_TICK cache index must fit for tick_spacing {tick_spacing}"
            );
        }
    }

    #[test]
    fn zero_tick_spacing_is_rejected_without_building_storage() {
        assert!(TickSlab::new(0).is_none());
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
    fn insert_and_get_roundtrips_protocol_boundary_ticks() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        let min = slab.indexer(TickIndex::MIN);
        let zero = slab.indexer(tick(0));
        let max = slab.indexer(TickIndex::MAX);

        slab.insert(min, info(1));
        slab.insert(zero, info(2));
        slab.insert(max, info(3));

        assert_eq!(slab.get(min).copied(), Some(info(1)));
        assert_eq!(slab.get(zero).copied(), Some(info(2)));
        assert_eq!(slab.get(max).copied(), Some(info(3)));
    }

    #[test]
    fn tick_spacing_compresses_ticks_into_pool_pages() {
        let mut slab = TickSlab::new(10).unwrap();
        let compressed_tick = slab.indexer(tick(160));
        let neighboring_tick = slab.indexer(tick(150));

        slab.insert(compressed_tick, info(42));

        assert_eq!(slab.get(compressed_tick).copied(), Some(info(42)));
        assert_eq!(slab.get(neighboring_tick), None);
        assert_ne!(compressed_tick, neighboring_tick);
    }

    #[test]
    fn unaligned_ticks_follow_the_same_spacing_compression_as_indexer() {
        let mut slab = TickSlab::new(10).unwrap();
        let aligned_tick = slab.indexer(tick(10));
        let unaligned_tick_in_same_bucket = slab.indexer(tick(19));

        slab.insert(aligned_tick, info(100));

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

        slab.insert(indexer, TickInfo::DEFAULT);

        assert_eq!(slab.get(indexer).copied(), Some(TickInfo::DEFAULT));
        assert_eq!(slab.values.len(), 1);
    }

    #[test]
    fn replacing_existing_tick_updates_value_without_allocating_another_page() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let indexer = slab.indexer(tick(7));

        slab.insert(indexer, info(100));
        slab.insert(indexer, info(250));

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
    fn removing_one_tick_keeps_other_ticks_in_the_same_page() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let first = slab.indexer(tick(0));
        let second = slab.indexer(tick(-1));
        assert_eq!(first.cache_index(), second.cache_index());

        slab.insert(first, info(100));
        slab.insert(second, info(200));
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
    fn removing_last_tick_frees_page_from_cache_bitmap_and_storage() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();
        let indexer = slab.indexer(tick(16));
        let cache_index = indexer.cache_index();

        slab.insert(indexer, info(100));
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

        slab.insert(indexer, info(100));
        slab.remove(indexer);
        slab.insert(indexer, info(300));

        assert_eq!(slab.get(indexer).copied(), Some(info(300)));
        assert_eq!(slab.values.len(), 1);
        assert!(slab.bitmap.contains(cache_index));
        assert_ne!(slab.cache[cache_index as usize], INITIAL_CACHE_VALUE);
    }

    #[test]
    fn pages_are_linked_in_cache_order_regardless_of_insert_order() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        for cache_index in [5, 1, 3] {
            let indexer = slab.indexer(cache_tick(cache_index));
            slab.insert(indexer, info(cache_index as u128));
        }

        assert_page_links(&slab, 1, None, Some(3));
        assert_page_links(&slab, 3, Some(1), Some(5));
        assert_page_links(&slab, 5, Some(3), None);
    }

    #[test]
    fn removing_middle_page_rewires_neighbor_pages() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        for cache_index in [1, 3, 5] {
            let indexer = slab.indexer(cache_tick(cache_index));
            slab.insert(indexer, info(cache_index as u128));
        }

        let middle = slab.indexer(cache_tick(3));
        slab.remove(middle);

        assert_page_absent(&slab, 3);
        assert_page_links(&slab, 1, None, Some(5));
        assert_page_links(&slab, 5, Some(1), None);
    }

    #[test]
    fn removing_head_and_tail_pages_preserves_remaining_chain() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        for cache_index in [1, 3, 5] {
            let indexer = slab.indexer(cache_tick(cache_index));
            slab.insert(indexer, info(cache_index as u128));
        }

        slab.remove(slab.indexer(cache_tick(1)));
        assert_page_absent(&slab, 1);
        assert_page_links(&slab, 3, None, Some(5));
        assert_page_links(&slab, 5, Some(3), None);

        slab.remove(slab.indexer(cache_tick(5)));
        assert_page_absent(&slab, 5);
        assert_page_links(&slab, 3, None, None);
    }

    #[test]
    fn next_and_prev_find_neighbors_inside_the_same_page() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        let slot_0 = slab.indexer(tick(0));
        let slot_1 = slab.indexer(tick(-1));
        let slot_4 = slab.indexer(tick(2));

        slab.insert(slot_0, info(10));
        slab.insert(slot_1, info(20));
        slab.insert(slot_4, info(40));

        assert_eq!(slab.next(slot_0).copied(), Some(info(20)));
        assert_eq!(slab.next(slot_1).copied(), Some(info(40)));
        assert_eq!(slab.prev(slot_4).copied(), Some(info(20)));
        assert_eq!(slab.prev(slot_1).copied(), Some(info(10)));
    }

    #[test]
    fn next_and_prev_cross_page_links() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        let page_0_last = slab.indexer(tick(-16));
        let page_1_first = slab.indexer(tick(16));
        let page_3_first = slab.indexer(tick(48));

        slab.insert(page_0_last, info(31));
        slab.insert(page_1_first, info(100));
        slab.insert(page_3_first, info(300));

        assert_eq!(slab.next(page_0_last).copied(), Some(info(100)));
        assert_eq!(slab.next(page_1_first).copied(), Some(info(300)));
        assert_eq!(slab.next(page_3_first), None);

        assert_eq!(slab.prev(page_3_first).copied(), Some(info(100)));
        assert_eq!(slab.prev(page_1_first).copied(), Some(info(31)));
        assert_eq!(slab.prev(page_0_last), None);
    }

    #[test]
    fn next_and_prev_skip_from_absent_pages() {
        let mut slab = TickSlab::new(TICK_SPACING).unwrap();

        let page_1_first = slab.indexer(tick(16));
        let absent_page_2_first = slab.indexer(tick(32));
        let page_3_first = slab.indexer(tick(48));

        slab.insert(page_1_first, info(100));
        slab.insert(page_3_first, info(300));

        assert_eq!(slab.next(absent_page_2_first).copied(), Some(info(300)));
        assert_eq!(slab.prev(absent_page_2_first).copied(), Some(info(100)));
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
                    slab.insert(indexer, tick_info);
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
}
