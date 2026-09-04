use crate::core::types::tick::TickIndex;

/// A compact, order-preserving key for addressing tick slab storage.
///
/// Ticks are first compressed by the pool's tick spacing, then projected into
/// a bounded unsigned range by flipping the sign bit inside the minimum bit width
/// needed for the compressed protocol tick range. That keeps signed tick order
/// intact in the slab key space: lower ticks have lower keys, higher ticks have
/// higher keys, and zero sits in the middle of the range.
///
/// The encoded key is then split into a `cache_index` identifying the 32-slot
/// slab page and a `slot_index` identifying the page-local slot. This gives the
/// tick store stable page/slot addressing while keeping traversal logic aligned
/// with protocol tick order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickSlabIndexer(u32);

impl TickSlabIndexer {
    /// Builds a slab indexer from a protocol tick and pool tick spacing.
    ///
    /// The tick is normalized with Euclidean division so negative unaligned
    /// ticks land in the same compressed bucket as protocol spacing checks.
    /// The compressed value is then encoded by flipping the sign bit of its
    /// spacing-adjusted representation. For `tick_spacing = 1`, this places
    /// `TickIndex::MIN` near the start of the key space, `0` at the midpoint,
    /// and `TickIndex::MAX` near the end.
    pub(crate) const fn from_tick(tick_idx: TickIndex, tick_spacing: u32) -> Self {
        let compressed = tick_idx.value().div_euclid(tick_spacing as i32);
        let x = compressed as u32;

        // Use only the bits required for the compressed protocol tick range.
        let spacing_bits = 31 - tick_spacing.leading_zeros();
        let bits = TickIndex::MAX_BITS - spacing_bits;

        let mask = u32::MAX >> (32 - bits);
        let sign = 1u32 << (bits - 1);

        Self((x & mask) ^ sign)
    }

    /// Decodes this slab indexer back into the protocol tick it represents.
    ///
    /// This is the inverse of [`Self::from_tick`] for indexes created with the
    /// same tick spacing. Callers must use the pool spacing associated with the
    /// slab; using a different spacing would decode the page/slot key in the
    /// wrong coordinate system.
    pub(crate) const fn to_tick(self, tick_spacing: u32) -> TickIndex {
        let spacing_bits = 31 - tick_spacing.leading_zeros();
        let bits = TickIndex::MAX_BITS - spacing_bits;

        let sign = 1u32 << (bits - 1);

        // Undo the order-preserving sign-bit flip from `from_tick`.
        let raw = self.0 ^ sign;

        // Restore the signed compressed tick from the spacing-adjusted width.
        let compressed = ((raw << (32 - bits)) as i32) >> (32 - bits);

        unsafe { TickIndex::new_unchecked(compressed * tick_spacing as i32) }
    }

    /// Builds an indexer from its slab page and page-local slot components.
    #[inline(always)]
    pub(crate) const fn from_parts(cache_index: u16, slot_index: u8) -> Self {
        Self(((cache_index as u32) << 5) | ((slot_index as u32) & 0x1F))
    }

    /// Returns bits `0..5` of the encoded value (range `0..32`), identifying
    /// the slot within a slab bucket.
    #[inline(always)]
    pub(crate) const fn slot_index(&self) -> u8 {
        (self.0 & 0x1F) as u8
    }

    /// Returns bits `5..21` of the encoded value (range `0..65536`),
    /// identifying which slab bucket/page this tick belongs to.
    #[inline(always)]
    pub(crate) const fn cache_index(&self) -> u16 {
        ((self.0 >> 5) & 0xFFFF) as u16
    }
}

#[cfg(test)]
impl TickSlabIndexer {
    pub(crate) const fn value(&self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_SPACING: u32 = 1;
    const SIGN_BIT: u32 = 1 << (TickIndex::MAX_BITS - 1);

    /// Helper to build a `TickIndex` from a raw `i32`.
    fn tick(value: i32) -> TickIndex {
        TickIndex::new(value).unwrap()
    }

    fn make_indexer(value: i32) -> TickSlabIndexer {
        TickSlabIndexer::from_tick(tick(value), TICK_SPACING)
    }

    // ---------------------------------------------------------------
    // Order-preserving encoding correctness
    // ---------------------------------------------------------------

    #[test]
    fn zero_maps_to_signed_range_midpoint() {
        assert_eq!(make_indexer(0).value(), SIGN_BIT);
    }

    #[test]
    fn small_values_match_sign_bit_flip_sequence() {
        let cases = [
            (-4, SIGN_BIT - 4),
            (-3, SIGN_BIT - 3),
            (-2, SIGN_BIT - 2),
            (-1, SIGN_BIT - 1),
            (0, SIGN_BIT),
            (1, SIGN_BIT + 1),
            (2, SIGN_BIT + 2),
            (3, SIGN_BIT + 3),
            (4, SIGN_BIT + 4),
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
    fn tick_spacing_normalizes_before_sign_bit_encoding() {
        const SPACING_SIGN_BIT: u32 = 1 << (TickIndex::MAX_BITS - 4);

        let cases = [
            (0, 10, SPACING_SIGN_BIT),
            (9, 10, SPACING_SIGN_BIT),
            (10, 10, SPACING_SIGN_BIT + 1),
            (20, 10, SPACING_SIGN_BIT + 2),
            (-1, 10, SPACING_SIGN_BIT - 1),
            (-10, 10, SPACING_SIGN_BIT - 1),
            (-11, 10, SPACING_SIGN_BIT - 2),
            (-20, 10, SPACING_SIGN_BIT - 2),
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
    fn sign_bit_encoding_is_injective_for_small_range() {
        // No two distinct inputs in a reasonable range should collide.
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for v in -1000..=1000 {
            let encoded = make_indexer(v).value();
            assert!(seen.insert(encoded), "collision detected for input {v}");
        }
    }

    #[test]
    fn protocol_extremes_match_known_values() {
        let min_encoded = TickSlabIndexer::from_tick(TickIndex::MIN, TICK_SPACING).value();
        let max_encoded = TickSlabIndexer::from_tick(TickIndex::MAX, TICK_SPACING).value();

        // TickIndex is bounded by the Uniswap tick range, not by i32::MIN/MAX.
        assert_eq!(min_encoded, 161_304);
        assert_eq!(max_encoded, 1_935_848);
    }

    #[test]
    fn sign_bit_encoding_preserves_tick_order() {
        for n in -500i32..500 {
            let lower = make_indexer(n).value();
            let higher = make_indexer(n + 1).value();
            assert!(
                lower < higher,
                "tick {n} should encode before tick {}",
                n + 1
            );
        }
    }

    // ---------------------------------------------------------------
    // slot_index / cache_index bit-field extraction
    // ---------------------------------------------------------------

    #[test]
    fn slot_index_matches_known_encoded_boundaries() {
        let cases = [
            (0, 0),
            (-1, 31),
            (15, 15),
            (-16, 16),
            (16, 16),
            (-17, 15),
            (*TickIndex::MIN, 24),
            (*TickIndex::MAX, 8),
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
            (0, 32_768),
            (-16, 32_767),
            (16, 32_768),
            (-17, 32_767),
            (31, 32_768),
            (-32, 32_767),
            (32, 32_769),
            (-33, 32_766),
            (*TickIndex::MIN, 5_040),
            (*TickIndex::MAX, 60_495),
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
        // Pick tick inputs whose encoded values land on known page/slot
        // boundaries without constructing a raw private indexer.
        let indexer = make_indexer(-32);
        assert_eq!(indexer.value(), SIGN_BIT - 32);
        assert_eq!(indexer.slot_index(), 0);
        assert_eq!(indexer.cache_index(), 32_767);

        let indexer = make_indexer(-16);
        assert_eq!(indexer.value(), SIGN_BIT - 16);
        assert_eq!(indexer.slot_index(), 16);
        assert_eq!(indexer.cache_index(), 32_767);

        let indexer = make_indexer(16);
        assert_eq!(indexer.value(), SIGN_BIT + 16);
        assert_eq!(indexer.slot_index(), 16);
        assert_eq!(indexer.cache_index(), 32_768);
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
    #[allow(clippy::clone_on_copy)]
    fn indexer_is_copy_and_clone() {
        let original = TickSlabIndexer::from_tick(tick(7), TICK_SPACING);
        let copied = original;
        let cloned = original.clone();
        assert_eq!(original, copied);
        assert_eq!(original, cloned);
    }
}
