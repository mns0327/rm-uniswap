use crate::{core::types::tick_slab_indexer::TickSlabIndexer, v4::TickInfo};

/// Fixed-size storage for one page of initialized pool ticks.
///
/// Each value owns 32 `TickInfo` slots and is linked to nearby initialized
/// pages through `prev_idx` / `next_idx`. The outer `TickSlab` uses this page
/// as the storage unit behind its cache, so neighboring active ticks can be
/// scanned with predictable memory access instead of allocating one node per
/// tick.
pub(crate) struct TickSlabValue {
    /// Tick payloads for this page.
    ///
    /// A slot can contain `TickInfo::default()` even when it is not logically
    /// present. Always consult `initialized_map` before exposing a slot to
    /// callers.
    slots: [TickInfo; 32],
    /// Occupancy bitmap for the 32 slots in `slots`.
    ///
    /// Bit `i` is set when slot `i` has been inserted into the slab. The bit
    /// represents logical presence, not value semantics; inserting
    /// `TickInfo::default()` still marks the slot as initialized, and removing
    /// a slot clears the bit even though the backing array remains allocated.
    initialized_map: u32,
    /// Cache index of the previous initialized page.
    ///
    /// `None` means this page is currently the head of the initialized-page
    /// chain maintained by `TickSlab`.
    prev_idx: Option<u16>,
    /// Cache index of the next initialized page.
    ///
    /// `None` means this page is currently the tail of the initialized-page
    /// chain maintained by `TickSlab`.
    next_idx: Option<u16>,
}

impl TickSlabValue {
    /// Creates an empty page with caller-supplied neighbor links.
    ///
    /// The page starts with no logically initialized ticks. Neighbor links are
    /// accepted here because `TickSlab` determines the page's position in the
    /// initialized-page chain before storing it in the backing allocator.
    pub fn new(prev_idx: Option<u16>, next_idx: Option<u16>) -> Self {
        Self {
            slots: [TickInfo::default(); 32],
            initialized_map: 0,
            prev_idx,
            next_idx,
        }
    }

    /// Returns the number of logically present ticks in this page.
    ///
    /// This is derived from the occupancy bitmap and is used by `TickSlab` to
    /// decide when an empty page can be unlinked and removed from storage.
    #[allow(dead_code)]
    #[inline(always)]
    pub fn initialized(&self) -> u8 {
        self.initialized_map.count_ones() as u8
    }

    /// Inserts or replaces the tick at the slot selected by `tick_indexer`.
    ///
    /// The caller is responsible for ensuring that the indexer belongs to this
    /// page. This method only writes the page-local slot and updates the
    /// occupancy bitmap.
    pub fn insert(&mut self, tick_indexer: &TickSlabIndexer, tick_info: TickInfo) {
        self.slots[tick_indexer.slot_index() as usize] = tick_info;
        self.initialized_map |= 1 << tick_indexer.slot_index();
    }

    /// Returns the tick stored at the slot selected by `tick_indexer`.
    ///
    /// Uninitialized slots are hidden even though the backing array contains a
    /// default value. This preserves the slab's invariant that only explicitly
    /// inserted ticks are visible to callers.
    pub fn get(&self, tick_indexer: &TickSlabIndexer) -> Option<&TickInfo> {
        if self.is_initialized(tick_indexer) {
            Some(&self.slots[tick_indexer.slot_index() as usize])
        } else {
            None
        }
    }

    /// Removes the tick at the slot selected by `tick_indexer`.
    ///
    /// Removal clears both the stored value and the occupancy bit. The outer
    /// slab is responsible for unlinking this page if the removal makes it
    /// empty.
    pub fn remove(&mut self, tick_indexer: &TickSlabIndexer) {
        self.slots[tick_indexer.slot_index() as usize] = TickInfo::DEFAULT;
        self.initialized_map &= !(1 << tick_indexer.slot_index());
    }

    /// Returns whether the selected slot is logically present.
    ///
    /// This is intentionally bitmap-based rather than value-based so a default
    /// `TickInfo` can still be a valid inserted value.
    #[inline(always)]
    fn is_initialized(&self, tick_indexer: &TickSlabIndexer) -> bool {
        self.initialized_map & (1 << tick_indexer.slot_index()) != 0
    }

    /// Updates the previous-page link after the outer slab relinks neighbors.
    #[inline(always)]
    pub fn set_prev_idx(&mut self, prev_idx: Option<u16>) {
        self.prev_idx = prev_idx;
    }

    /// Updates the next-page link after the outer slab relinks neighbors.
    #[inline(always)]
    pub fn set_next_idx(&mut self, next_idx: Option<u16>) {
        self.next_idx = next_idx;
    }

    /// Returns the previous initialized page in the slab chain.
    #[inline(always)]
    pub fn prev_idx(&self) -> Option<u16> {
        self.prev_idx
    }

    /// Returns the next initialized page in the slab chain.
    #[inline(always)]
    pub fn next_idx(&self) -> Option<u16> {
        self.next_idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::tick::TickIndex;

    const TICK_SPACING: u32 = 1;

    fn indexer(tick: i32) -> TickSlabIndexer {
        TickSlabIndexer::from_tick(TickIndex::new(tick).unwrap(), TICK_SPACING)
    }

    fn info(liquidity_gross: u128) -> TickInfo {
        TickInfo {
            liquidity_gross,
            ..TickInfo::DEFAULT
        }
    }

    #[test]
    fn new_page_starts_empty_and_keeps_neighbor_links() {
        let page = TickSlabValue::new(Some(7), Some(11));

        assert_eq!(page.initialized(), 0);
        assert_eq!(page.get(&indexer(0)), None);
        assert_eq!(page.prev_idx(), Some(7));
        assert_eq!(page.next_idx(), Some(11));
    }

    #[test]
    fn inserted_default_tick_is_still_logically_present() {
        let mut page = TickSlabValue::new(None, None);
        let slot = indexer(0);

        page.insert(&slot, TickInfo::DEFAULT);

        assert_eq!(page.initialized(), 1);
        assert_eq!(page.get(&slot).copied(), Some(TickInfo::DEFAULT));
    }

    #[test]
    fn replacing_a_slot_keeps_one_logical_tick() {
        let mut page = TickSlabValue::new(None, None);
        let slot = indexer(0);

        page.insert(&slot, info(100));
        page.insert(&slot, info(200));

        assert_eq!(page.initialized(), 1);
        assert_eq!(page.get(&slot).copied(), Some(info(200)));
    }

    #[test]
    fn removing_one_slot_does_not_affect_neighboring_slots() {
        let mut page = TickSlabValue::new(None, None);
        let first_slot = indexer(0);
        let second_slot = indexer(-1);

        page.insert(&first_slot, info(100));
        page.insert(&second_slot, info(200));
        page.remove(&first_slot);

        assert_eq!(page.initialized(), 1);
        assert_eq!(page.get(&first_slot), None);
        assert_eq!(page.get(&second_slot).copied(), Some(info(200)));
    }

    #[test]
    fn removing_the_last_slot_makes_the_page_empty() {
        let mut page = TickSlabValue::new(None, None);
        let slot = indexer(0);

        page.insert(&slot, info(100));
        page.remove(&slot);

        assert_eq!(page.initialized(), 0);
        assert_eq!(page.get(&slot), None);
    }

    #[test]
    fn neighbor_links_can_be_rewired_by_the_outer_slab() {
        let mut page = TickSlabValue::new(Some(1), Some(3));

        page.set_prev_idx(None);
        page.set_next_idx(Some(5));

        assert_eq!(page.prev_idx(), None);
        assert_eq!(page.next_idx(), Some(5));
    }
}
