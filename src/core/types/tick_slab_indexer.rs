use crate::core::types::tick::TickIndex;

/// A compact, cache-friendly encoding of a `TickIndex` used to index into
/// the tick slab storage.
///
/// The signed tick value is remapped to an unsigned `u32` via zigzag
/// encoding, so that ticks with small absolute magnitude (the common case,
/// close to the current price) map to small unsigned values. This keeps
/// related ticks physically close together in the slab, which improves
/// cache locality and lets the encoded value be split into a `cache_index`
/// (which slab page/bucket) and a `slot_index` (which slot within that
/// bucket).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TickSlabIndexer(u32);

impl TickSlabIndexer {
    /// Encodes a `TickIndex` into its zigzag `u32` representation.
    ///
    /// The tick is first normalized by `tick_spacing` (via `div_euclid`,
    /// so it rounds toward negative infinity rather than toward zero),
    /// then zigzag-encoded so that values with small absolute magnitude —
    /// positive or negative — map to small unsigned values, e.g.
    /// `0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, ...`.
    pub(crate) const fn from_tick(tick_idx: TickIndex, tick_spacing: u32) -> Self {
        let compressed = tick_idx.value().div_euclid(tick_spacing as i32);

        let zigzag = ((compressed << 1) ^ (compressed >> 31)) as u32;
        Self(zigzag)
    }

    /// Builds an indexer from its slab page and page-local slot components.
    #[inline(always)]
    pub(crate) const fn from_parts(cache_index: u16, slot_index: u8) -> Self {
        Self(((cache_index as u32) << 5) | ((slot_index as u32) & 0x1F))
    }

    /// Returns the raw zigzag-encoded `u32` value.
    #[inline(always)]
    pub(crate) fn value(&self) -> u32 {
        self.0
    }

    /// Returns bits `0..5` of the encoded value (range `0..32`), identifying
    /// the slot within a slab bucket.
    #[inline(always)]
    pub(crate) fn slot_index(&self) -> u8 {
        (self.0 & 0x1F) as u8
    }

    /// Returns bits `5..21` of the encoded value (range `0..65536`),
    /// identifying which slab bucket/page this tick belongs to.
    #[inline(always)]
    pub(crate) fn cache_index(&self) -> u16 {
        ((self.0 >> 5) & 0xFFFF) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_SPACING: u32 = 1;

    /// Helper to build a `TickIndex` from a raw `i32`.
    fn tick(value: i32) -> TickIndex {
        TickIndex::new(value).unwrap()
    }

    fn make_indexer(value: i32) -> TickSlabIndexer {
        TickSlabIndexer::from_tick(tick(value), TICK_SPACING)
    }

    // ---------------------------------------------------------------
    // Zigzag encoding correctness
    // ---------------------------------------------------------------

    #[test]
    fn zigzag_zero_maps_to_zero() {
        assert_eq!(make_indexer(0).value(), 0);
    }

    #[test]
    fn zigzag_small_values_match_known_sequence() {
        // 0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, 2 -> 4, -3 -> 5 ...
        let cases = [
            (0, 0u32),
            (-1, 1),
            (1, 2),
            (-2, 3),
            (2, 4),
            (-3, 5),
            (3, 6),
            (-4, 7),
            (4, 8),
        ];
        for (input, expected) in cases {
            assert_eq!(
                make_indexer(input).value(),
                expected,
                "mismatch for input {input}"
            );
        }
    }

    #[test]
    fn tick_spacing_normalizes_before_zigzag_encoding() {
        let cases = [
            (0, 10, 0u32), // 0 / 10 = 0, zigzag(0) = 0
            (9, 10, 0),    // 9 / 10 = 0, zigzag(0) = 0
            (10, 10, 2),   // 10 / 10 = 1, zigzag(1) = 2
            (20, 10, 4),   // 20 / 10 = 2, zigzag(2) = 4
            (-1, 10, 1),   // -1 div_euclid 10 = -1, zigzag(-1) = 1
            (-10, 10, 1),  // -10 / 10 = -1, zigzag(-1) = 1
            (-11, 10, 3),  // -11 div_euclid 10 = -2, zigzag(-2) = 3
            (-20, 10, 3),  // -20 / 10 = -2, zigzag(-2) = 3
        ];

        for (input, tick_spacing, expected) in cases {
            assert_eq!(
                TickSlabIndexer::from_tick(tick(input), tick_spacing).value(),
                expected,
                "mismatch for input {input} with tick_spacing {tick_spacing}"
            );
        }
    }

    #[test]
    fn zigzag_is_injective_for_small_range() {
        // No two distinct inputs in a reasonable range should collide.
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for v in -1000..=1000 {
            let encoded = make_indexer(v).value();
            assert!(seen.insert(encoded), "collision detected for input {v}");
        }
    }

    #[test]
    fn zigzag_protocol_extremes_match_known_values() {
        let min_encoded = TickSlabIndexer::from_tick(TickIndex::MIN, TICK_SPACING).value();
        let max_encoded = TickSlabIndexer::from_tick(TickIndex::MAX, TICK_SPACING).value();

        // TickIndex is bounded by the Uniswap tick range, not by i32::MIN/MAX.
        assert_eq!(max_encoded, 1_774_544);
        assert_eq!(min_encoded, 1_774_543);
    }

    #[test]
    fn zigzag_preserves_sign_alternation_pattern() {
        // For any n >= 0: encode(n) is even, encode(-n-1) is odd.
        for n in 0..500i32 {
            let pos = make_indexer(n).value();
            assert_eq!(
                pos % 2,
                0,
                "positive tick {n} should encode to an even value"
            );

            if n < i32::MAX {
                let neg = make_indexer(-(n + 1)).value();
                assert_eq!(
                    neg % 2,
                    1,
                    "negative tick {} should encode to an odd value",
                    -(n + 1)
                );
            }
        }
    }

    // ---------------------------------------------------------------
    // slot_index / cache_index bit-field extraction
    // ---------------------------------------------------------------

    #[test]
    fn slot_index_matches_known_encoded_boundaries() {
        let cases = [
            (0, 0),
            (-1, 1),
            (15, 30),
            (-16, 31),
            (16, 0),
            (-17, 1),
            (*TickIndex::MIN, 15),
            (*TickIndex::MAX, 16),
        ];

        for (input, expected_slot) in cases {
            let indexer = make_indexer(input);
            assert_eq!(
                indexer.slot_index(),
                expected_slot,
                "mismatch for input {input}"
            );
        }
    }

    #[test]
    fn cache_index_matches_known_bucket_boundaries() {
        let cases = [
            (0, 0),
            (-16, 0),
            (16, 1),
            (-17, 1),
            (31, 1),
            (-32, 1),
            (32, 2),
            (-33, 2),
            (*TickIndex::MIN, 55_454),
            (*TickIndex::MAX, 55_454),
        ];

        for (input, expected_cache) in cases {
            let indexer = make_indexer(input);
            assert_eq!(
                indexer.cache_index(),
                expected_cache,
                "mismatch for input {input}"
            );
        }
    }

    #[test]
    fn slot_and_cache_index_reconstruct_low_21_bits_of_value() {
        for n in [-70000, -1, 0, 1, 12345, *TickIndex::MAX, *TickIndex::MIN] {
            let indexer = make_indexer(n);
            let reconstructed =
                (indexer.slot_index() as u32) | ((indexer.cache_index() as u32) << 5);
            let low_21_bits = indexer.value() & 0x1F_FFFF; // bits 0..=20
            assert_eq!(reconstructed, low_21_bits);
        }
    }

    #[test]
    fn slot_index_zero_and_max_boundary() {
        // value = 0b11111 (31) -> slot_index 31, cache_index 0
        // We can't directly construct a TickSlabIndexer from a raw u32
        // (private field), so instead pick tick inputs whose zigzag
        // encoding lands on known boundary values.
        // zigzag(15) = 30 (0b11110) -> slot 30
        let indexer = make_indexer(15);
        assert_eq!(indexer.value(), 30);
        assert_eq!(indexer.slot_index(), 30);
        assert_eq!(indexer.cache_index(), 0);

        // zigzag(-16) = 31 (0b11111) -> slot 31
        let indexer = make_indexer(-16);
        assert_eq!(indexer.value(), 31);
        assert_eq!(indexer.slot_index(), 31);
        assert_eq!(indexer.cache_index(), 0);

        // zigzag(16) = 32 (0b100000) -> slot 0, cache_index 1
        let indexer = make_indexer(16);
        assert_eq!(indexer.value(), 32);
        assert_eq!(indexer.slot_index(), 0);
        assert_eq!(indexer.cache_index(), 1);
    }

    #[test]
    fn slot_index_is_always_within_bucket_width_for_valid_ticks() {
        for n in [
            *TickIndex::MIN,
            -100_000,
            -1,
            0,
            1,
            100_000,
            *TickIndex::MAX,
        ] {
            let indexer = make_indexer(n);
            assert!(indexer.slot_index() < 32, "slot_index must be in 0..32");
        }
    }

    // ---------------------------------------------------------------
    // Trait / API surface
    // ---------------------------------------------------------------

    #[test]
    fn equal_compressed_ticks_produce_equal_indexers_for_same_spacing() {
        assert_eq!(
            TickSlabIndexer::from_tick(tick(10), 10),
            TickSlabIndexer::from_tick(tick(19), 10)
        );
    }

    #[test]
    fn equal_ticks_produce_equal_indexers() {
        assert_eq!(
            TickSlabIndexer::from_tick(tick(42), TICK_SPACING),
            TickSlabIndexer::from_tick(tick(42), TICK_SPACING)
        );
    }

    #[test]
    fn different_ticks_produce_different_indexers() {
        assert_ne!(
            TickSlabIndexer::from_tick(tick(42), TICK_SPACING),
            TickSlabIndexer::from_tick(tick(43), TICK_SPACING)
        );
    }

    #[test]
    fn indexer_is_copy_and_clone() {
        let original = TickSlabIndexer::from_tick(tick(7), TICK_SPACING);
        let copied = original;
        let cloned = original.clone();
        assert_eq!(original, copied);
        assert_eq!(original, cloned);
    }
}
