use crate::core::types::tick::TickIndex;

/// A compact, cache-friendly encoding of a `TickIndex` used to index into
/// the tick slab storage.
///
/// The signed tick value is remapped to an unsigned `u32` via zigzag
/// encoding so that small-magnitude ticks (the common case, close to the
/// current price) map to small unsigned values. This keeps related ticks
/// physically close together in the slab, improving cache locality and
/// allowing the encoded value to be split into a `cache_index` (which
/// slab page/bucket) and a `slot_index` (which slot within that bucket).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TickSlabIndexer(u32);

impl TickSlabIndexer {
    /// Encodes a `TickIndex` into its zigzag `u32` representation.
    ///
    /// Zigzag encoding maps signed integers to unsigned integers so that
    /// values with small absolute magnitude (positive or negative) end up
    /// with small unsigned values, e.g. `0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, ...`.
    pub(crate) const fn from_tick(tick_idx: TickIndex) -> Self {
        let value = tick_idx.value();
        let zigzag = ((value << 1) ^ (value >> 31)) as u32;
        Self(zigzag)
    }

    /// Returns the raw encoded `u32` value.
    #[inline(always)]
    pub(crate) fn value(&self) -> u32 {
        self.0
    }

    /// Returns the low 5 bits (0..32) of the encoded value, identifying
    /// the slot within a slab bucket.
    #[inline(always)]
    pub(crate) fn slot_index(&self) -> u8 {
        (self.0 & 0x1F) as u8
    }

    /// Returns bits 5..21 of the encoded value, identifying which slab
    /// bucket/page this tick belongs to.
    #[inline(always)]
    pub(crate) fn cache_index(&self) -> u16 {
        ((self.0 >> 5) & 0xFFFF) as u16
    }
}

/// Convenience conversion so a `TickIndex` can be encoded into a
/// `TickSlabIndexer` via `.into()` wherever a `From`/`Into` bound is more
/// idiomatic than calling `from_tick` directly.
impl From<TickIndex> for TickSlabIndexer {
    fn from(tick_idx: TickIndex) -> Self {
        Self::from_tick(tick_idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a `TickIndex` from a raw `i32`.
    fn tick(value: i32) -> TickIndex {
        TickIndex::new(value).unwrap()
    }

    // ---------------------------------------------------------------
    // Zigzag encoding correctness
    // ---------------------------------------------------------------

    #[test]
    fn zigzag_zero_maps_to_zero() {
        assert_eq!(TickSlabIndexer::from_tick(tick(0)).value(), 0);
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
                TickSlabIndexer::from_tick(tick(input)).value(),
                expected,
                "mismatch for input {input}"
            );
        }
    }

    #[test]
    fn zigzag_is_injective_for_small_range() {
        // No two distinct inputs in a reasonable range should collide.
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for v in -1000..=1000 {
            let encoded = TickSlabIndexer::from_tick(tick(v)).value();
            assert!(seen.insert(encoded), "collision detected for input {v}");
        }
    }

    #[test]
    fn zigzag_protocol_extremes_match_known_values() {
        let min_encoded = TickSlabIndexer::from_tick(TickIndex::MIN).value();
        let max_encoded = TickSlabIndexer::from_tick(TickIndex::MAX).value();

        // TickIndex is bounded by the Uniswap tick range, not by i32::MIN/MAX.
        assert_eq!(max_encoded, 1_774_544);
        assert_eq!(min_encoded, 1_774_543);
    }

    #[test]
    fn zigzag_preserves_sign_alternation_pattern() {
        // For any n >= 0: encode(n) is even, encode(-n-1) is odd.
        for n in 0..500i32 {
            let pos = TickSlabIndexer::from_tick(tick(n)).value();
            assert_eq!(
                pos % 2,
                0,
                "positive tick {n} should encode to an even value"
            );

            if n < i32::MAX {
                let neg = TickSlabIndexer::from_tick(tick(-(n + 1))).value();
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
            let indexer = TickSlabIndexer::from_tick(tick(input));
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
            let indexer = TickSlabIndexer::from_tick(tick(input));
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
            let indexer = TickSlabIndexer::from_tick(tick(n));
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
        let indexer = TickSlabIndexer::from_tick(tick(15));
        assert_eq!(indexer.value(), 30);
        assert_eq!(indexer.slot_index(), 30);
        assert_eq!(indexer.cache_index(), 0);

        // zigzag(-16) = 31 (0b11111) -> slot 31
        let indexer = TickSlabIndexer::from_tick(tick(-16));
        assert_eq!(indexer.value(), 31);
        assert_eq!(indexer.slot_index(), 31);
        assert_eq!(indexer.cache_index(), 0);

        // zigzag(16) = 32 (0b100000) -> slot 0, cache_index 1
        let indexer = TickSlabIndexer::from_tick(tick(16));
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
            let indexer = TickSlabIndexer::from_tick(tick(n));
            assert!(indexer.slot_index() < 32, "slot_index must be in 0..32");
        }
    }

    // ---------------------------------------------------------------
    // Trait / API surface
    // ---------------------------------------------------------------

    #[test]
    fn from_trait_matches_from_tick() {
        for n in [-999, -1, 0, 1, 999] {
            let via_from_tick = TickSlabIndexer::from_tick(tick(n));
            let via_into: TickSlabIndexer = tick(n).into();
            assert_eq!(via_from_tick, via_into);
        }
    }

    #[test]
    fn equal_ticks_produce_equal_indexers() {
        assert_eq!(
            TickSlabIndexer::from_tick(tick(42)),
            TickSlabIndexer::from_tick(tick(42))
        );
    }

    #[test]
    fn different_ticks_produce_different_indexers() {
        assert_ne!(
            TickSlabIndexer::from_tick(tick(42)),
            TickSlabIndexer::from_tick(tick(43))
        );
    }

    #[test]
    fn indexer_is_copy_and_clone() {
        let original = TickSlabIndexer::from_tick(tick(7));
        let copied = original;
        let cloned = original.clone();
        assert_eq!(original, copied);
        assert_eq!(original, cloned);
    }
}
