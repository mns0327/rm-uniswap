use ruint::aliases::U256;

use crate::{core::types::tick_slab_indexer::TickSlabIndexer, v4::TickInfo};

/// Fixed-size storage for one page of initialized pool ticks.
///
/// Each value owns 32 `TickInfo` slots and is linked to nearby initialized
/// pages through `prev_idx` / `next_idx`. The outer `TickSlab` uses this page
/// as the storage unit behind its cache, so neighboring active ticks can be
/// scanned with predictable memory access instead of allocating one node per
/// tick.
#[derive(Debug)]
pub(crate) struct TickSlabValue {
    /// Tick payloads for this page.
    ///
    /// A slot can contain `TickInfo::default()` even when it is not logically
    /// present. Always consult `initialized_map` before exposing a slot to
    /// callers.
    slots: [Option<TickInfo>; 32],
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
            slots: [None; 32],
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
        self.slots[tick_indexer.slot_index() as usize] = Some(tick_info);
        self.initialized_map |= 1 << tick_indexer.slot_index();
    }

    /// Returns the tick stored at the slot selected by `tick_indexer`.
    ///
    /// Uninitialized slots are hidden even though the backing array contains a
    /// default value. This preserves the slab's invariant that only explicitly
    /// inserted ticks are visible to callers.
    pub fn get(&self, tick_indexer: &TickSlabIndexer) -> Option<&TickInfo> {
        if self.is_initialized(tick_indexer) {
            self.slots[tick_indexer.slot_index() as usize].as_ref()
        } else {
            None
        }
    }

    /// Returns the mutable tick stored at the slot selected by `tick_indexer`.
    ///
    /// Like [`get`](Self::get), this only exposes logically initialized slots.
    pub(crate) fn get_mut(&mut self, tick_indexer: &TickSlabIndexer) -> Option<&mut TickInfo> {
        if self.is_initialized(tick_indexer) {
            self.slots[tick_indexer.slot_index() as usize].as_mut()
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
        self.slots[tick_indexer.slot_index() as usize] = None;
        self.initialized_map &= !(1 << tick_indexer.slot_index());
    }

    /// Returns the next initialized tick in this page after `tick_indexer`.
    ///
    /// The occupancy map is already a compact 32-bit index, so traversal keeps
    /// only bits above the current slot and uses `trailing_zeros()` to jump
    /// directly to the least-significant remaining bit. That count is exactly
    /// the next initialized slot, with no per-slot scan.
    pub fn next(&self, tick_indexer: &TickSlabIndexer) -> Option<&TickInfo> {
        self.next_slot(tick_indexer).map(|(_, tick_info)| tick_info)
    }

    /// Returns the next initialized page-local slot and tick after `tick_indexer`.
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
        self.slots[next_slot as usize]
            .as_ref()
            .map(|tick_info| (next_slot, tick_info))
    }

    /// Returns the previous initialized tick in this page before `tick_indexer`.
    ///
    /// This mirrors [`next`](Self::next): keep only bits below the current
    /// slot, then use `leading_zeros()` to locate the most-significant
    /// remaining bit. Subtracting the zero count from the word width gives the
    /// previous initialized slot in constant time.
    pub fn prev(&self, tick_indexer: &TickSlabIndexer) -> Option<&TickInfo> {
        self.prev_slot(tick_indexer).map(|(_, tick_info)| tick_info)
    }

    /// Returns the previous initialized page-local slot and tick before `tick_indexer`.
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
        self.slots[prev_slot as usize]
            .as_ref()
            .map(|tick_info| (prev_slot, tick_info))
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
        self.slots[first_slot as usize]
            .as_ref()
            .map(|tick_info| (first_slot, tick_info))
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
        self.slots[last_slot as usize]
            .as_ref()
            .map(|tick_info| (last_slot, tick_info))
    }

    /// Iterates over all logically initialized slots in this page.
    pub(crate) fn initialized_slots(&self) -> impl Iterator<Item = (u8, &TickInfo)> + '_ {
        (0..32).filter_map(|slot| {
            if self.initialized_map & (1u32 << slot) != 0 {
                self.slots[slot]
                    .as_ref()
                    .map(|tick_info| (slot as u8, tick_info))
            } else {
                None
            }
        })
    }

    pub(crate) fn cross_fee_growth(
        &mut self,
        tick_indexer: &TickSlabIndexer,
        fee_growth_global0_x128: U256,
        fee_growth_global1_x128: U256,
    ) -> bool {
        if let Some(tick_info) = self.slots[tick_indexer.slot_index() as usize].as_mut() {
            tick_info.fee_growth_outside0_x128 =
                fee_growth_global0_x128.wrapping_sub(tick_info.fee_growth_outside0_x128);
            tick_info.fee_growth_outside1_x128 =
                fee_growth_global1_x128.wrapping_sub(tick_info.fee_growth_outside1_x128);
            true
        } else {
            false
        }
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
    fn next_and_prev_use_initialized_bits_inside_one_page() {
        let mut page = TickSlabValue::new(None, None);

        let slot_0 = indexer(0);
        let slot_1 = indexer(-1);
        let slot_4 = indexer(2);
        let slot_31 = indexer(-16);

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

        page.insert(&indexer(-1), info(20));
        page.insert(&indexer(15), info(300));

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
