use crate::core::types::{TickInfo, tick_slab_indexer::TickSlabIndexer};

/// Fixed-size storage for one cache page of initialized pool ticks.
///
/// A page owns 32 page-local slots. `initialized_map` is the visibility index
/// for those slots, and `prev_idx` / `next_idx` link this page to the previous
/// and next non-empty cache pages maintained by `TickSlab`. This keeps common
/// swap traversal local: first jump inside the 32-bit occupancy map, then cross
/// to an already-linked neighbor page only when needed.
#[derive(Debug)]
pub(crate) struct TickSlabValue {
    /// Tick payload storage for the 32 page-local slots.
    ///
    /// Uninitialized slots may contain `TickInfo::DEFAULT` or a stale value left
    /// by a previous removal. `initialized_map` is the source of truth for
    /// whether a slot is logically present.
    slots: [TickInfo; 32],
    /// Occupancy bitmap for the 32 slots in `slots`.
    ///
    /// Bit `i` is set when slot `i` is logically initialized. The bitmap is the
    /// compact index used for constant-time first/last/next/previous lookups,
    /// while the array stores the corresponding `TickInfo` payload.
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
    /// The page starts with no initialized slots. Neighbor links are accepted
    /// here because `TickSlab` owns page ordering and may know the surrounding
    /// pages before this value is placed in the slab allocator.
    pub fn new(prev_idx: Option<u16>, next_idx: Option<u16>) -> Self {
        Self {
            slots: [TickInfo::DEFAULT; 32],
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

    /// Inserts or replaces the tick at the page-local slot selected by `tick_indexer`.
    ///
    /// The caller is responsible for ensuring that the indexer belongs to this
    /// page. Replacing an already-initialized slot updates the payload without
    /// changing the initialized-slot count.
    pub fn insert(&mut self, tick_indexer: &TickSlabIndexer, tick_info: TickInfo) -> &TickInfo {
        self.slots[tick_indexer.slot_index() as usize] = tick_info;
        self.initialized_map |= 1 << tick_indexer.slot_index();
        &self.slots[tick_indexer.slot_index() as usize]
    }

    /// Returns the tick stored at the page-local slot selected by `tick_indexer`.
    ///
    /// A value is returned only when the occupancy bit is set. This preserves
    /// the slab invariant that only explicitly inserted ticks are visible.
    pub fn get(&self, tick_indexer: &TickSlabIndexer) -> Option<&TickInfo> {
        if self.is_initialized(tick_indexer) {
            Some(&self.slots[tick_indexer.slot_index() as usize])
        } else {
            None
        }
    }

    /// Returns the mutable tick stored at the page-local slot selected by `tick_indexer`.
    ///
    /// Like [`get`](Self::get), this only exposes logically initialized slots.
    pub(crate) fn get_mut(&mut self, tick_indexer: &TickSlabIndexer) -> Option<&mut TickInfo> {
        if self.is_initialized(tick_indexer) {
            Some(&mut self.slots[tick_indexer.slot_index() as usize])
        } else {
            None
        }
    }

    /// Removes the tick at the page-local slot selected by `tick_indexer`.
    ///
    /// Removal clears only the occupancy bit. The backing payload is left in
    /// place because `initialized_map` is the visibility guard. The outer slab
    /// is responsible for unlinking and freeing this page if the removal makes
    /// it empty.
    pub fn remove(&mut self, tick_indexer: &TickSlabIndexer) {
        self.initialized_map &= !(1 << tick_indexer.slot_index());
    }

    /// Returns the next initialized tick in this page after `tick_indexer`.
    ///
    /// The lookup is strictly page-local and excludes the current slot. The
    /// occupancy map keeps only bits above the current slot, then
    /// `trailing_zeros()` jumps directly to the lowest remaining set bit.
    pub fn next(&self, tick_indexer: &TickSlabIndexer) -> Option<&TickInfo> {
        self.next_slot(tick_indexer).map(|(_, tick_info)| tick_info)
    }

    /// Returns the next initialized page-local slot and tick after `tick_indexer`.
    ///
    /// The returned slot is greater than the current page-local slot. If no such
    /// bit exists, callers must move to `next_idx` themselves.
    pub(crate) fn next_slot(&self, tick_indexer: &TickSlabIndexer) -> Option<(u8, &TickInfo)> {
        let slot = tick_indexer.slot_index();

        if slot == 31 {
            return None;
        }

        let candidates = self.initialized_map & (u32::MAX << (slot + 1));

        if candidates == 0 {
            return None;
        }

        let next_slot = candidates.trailing_zeros() as u8;
        Some((next_slot, &self.slots[next_slot as usize]))
    }

    /// Returns the previous initialized tick in this page before `tick_indexer`.
    ///
    /// The lookup is strictly page-local and excludes the current slot. It keeps
    /// only bits below the current slot, then uses `leading_zeros()` to locate
    /// the highest remaining set bit.
    pub fn prev(&self, tick_indexer: &TickSlabIndexer) -> Option<&TickInfo> {
        self.prev_slot(tick_indexer).map(|(_, tick_info)| tick_info)
    }

    /// Returns the previous initialized page-local slot and tick before `tick_indexer`.
    ///
    /// The returned slot is less than the current page-local slot. If no such
    /// bit exists, callers must move to `prev_idx` themselves.
    pub(crate) fn prev_slot(&self, tick_indexer: &TickSlabIndexer) -> Option<(u8, &TickInfo)> {
        let slot = tick_indexer.slot_index();

        if slot == 0 {
            return None;
        }

        let candidates = self.initialized_map & ((1 << slot) - 1);

        if candidates == 0 {
            return None;
        }

        let prev_slot = (u32::BITS - 1 - candidates.leading_zeros()) as u8;
        Some((prev_slot, &self.slots[prev_slot as usize]))
    }

    /// Returns the first initialized tick in this page.
    ///
    /// `trailing_zeros()` points at the lowest set bit, which is the first
    /// initialized slot in page-local order.
    pub fn first(&self) -> Option<&TickInfo> {
        self.first_slot().map(|(_, tick_info)| tick_info)
    }

    /// Returns the first initialized page-local slot and tick in this page.
    pub(crate) fn first_slot(&self) -> Option<(u8, &TickInfo)> {
        if self.initialized_map == 0 {
            return None;
        }

        let first_slot = self.initialized_map.trailing_zeros() as u8;
        Some((first_slot, &self.slots[first_slot as usize]))
    }

    /// Returns the last initialized tick in this page.
    ///
    /// `leading_zeros()` counts the empty high bits before the highest set bit;
    /// converting that count back into a bit index gives the last initialized
    /// slot in page-local order.
    pub fn last(&self) -> Option<&TickInfo> {
        self.last_slot().map(|(_, tick_info)| tick_info)
    }

    /// Returns the last initialized page-local slot and tick in this page.
    pub(crate) fn last_slot(&self) -> Option<(u8, &TickInfo)> {
        if self.initialized_map == 0 {
            return None;
        }

        let last_slot = (u32::BITS - 1 - self.initialized_map.leading_zeros()) as u8;
        Some((last_slot, &self.slots[last_slot as usize]))
    }

    /// Iterates over all initialized slots in ascending page-local slot order.
    pub(crate) fn initialized_slots(&self) -> impl Iterator<Item = (u8, &TickInfo)> + '_ {
        (0..32).filter_map(|slot| {
            if self.initialized_map & (1u32 << slot) != 0 {
                Some((slot as u8, &self.slots[slot]))
            } else {
                None
            }
        })
    }

    /// Returns whether the selected page-local slot is logically initialized.
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
    use crate::core::types::{TickInfoInner, liquidity::Liquidity, tick::TickIndex};

    const TICK_SPACING: u32 = 1;

    fn indexer(tick: i32) -> TickSlabIndexer {
        TickSlabIndexer::from_tick(TickIndex::new(tick).unwrap(), TICK_SPACING)
    }

    fn page_slot(slot: u8) -> TickSlabIndexer {
        TickSlabIndexer::from_parts(7, slot)
    }

    fn info(liquidity_gross: u128) -> TickInfo {
        TickInfo {
            inner: TickInfoInner {
                liquidity_gross: Liquidity::new(liquidity_gross),
                ..TickInfoInner::DEFAULT
            },
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
    fn next_and_prev_use_initialized_bits_inside_one_page() {
        let mut page = TickSlabValue::new(None, None);

        let slot_0 = page_slot(0);
        let slot_1 = page_slot(1);
        let slot_4 = page_slot(4);
        let slot_31 = page_slot(31);

        page.insert(&slot_0, info(10));
        page.insert(&slot_1, info(20));
        page.insert(&slot_4, info(40));
        page.insert(&slot_31, info(310));

        assert_eq!(page.next(&slot_0).copied(), Some(info(20)));
        assert_eq!(page.next(&slot_1).copied(), Some(info(40)));
        assert_eq!(page.next(&slot_4).copied(), Some(info(310)));
        assert_eq!(page.next(&slot_31), None);

        assert_eq!(page.prev(&slot_31).copied(), Some(info(40)));
        assert_eq!(page.prev(&slot_4).copied(), Some(info(20)));
        assert_eq!(page.prev(&slot_1).copied(), Some(info(10)));
        assert_eq!(page.prev(&slot_0), None);
    }

    #[test]
    fn first_and_last_return_page_extremes() {
        let mut page = TickSlabValue::new(None, None);

        page.insert(&page_slot(1), info(20));
        page.insert(&page_slot(15), info(300));

        assert_eq!(page.first().copied(), Some(info(20)));
        assert_eq!(page.last().copied(), Some(info(300)));
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
