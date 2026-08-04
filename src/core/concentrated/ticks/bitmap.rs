// The bitmap is stored in `u64` words so word selection can be implemented
// with cheap shifts and masks instead of division and modulo.
const WORD_BITS: usize = u64::BITS as usize;
const WORD_SHIFT: usize = 6;
const WORD_MASK: usize = WORD_BITS - 1;

/// Fixed-capacity hierarchical bitmap for a dense `u16` key space.
///
/// `layer0` stores the actual key membership bits. `layer1` summarizes which
/// `layer0` words are non-empty, and `layer2` summarizes which `layer1` words
/// are non-empty. This keeps membership updates local while allowing nearest
/// initialized-key searches to skip whole empty words in a bounded number of
/// word operations.
///
/// The capacity is fixed at construction time and valid keys are always in the
/// half-open range `[0, cap)`. Because the input key space is `u16`, the top
/// summary fits in a single `u64`.
#[derive(Debug, Clone)]
pub struct HierBitmap {
    /// Exclusive upper bound for valid keys.
    cap: u16,

    /// Top-level summary: bit `i` is set when `layer1[i]` contains any key.
    layer2: u64,

    /// Mid-level summary: bit `j` in `layer1[i]` is set when the corresponding
    /// `layer0` word contains at least one key.
    layer1: Vec<u64>,

    /// Leaf storage: bit `j` in `layer0[i]` is set when key `i * 64 + j` is
    /// present.
    layer0: Vec<u64>,
}

impl HierBitmap {
    /// Creates an empty bitmap for keys in `[0, cap)`.
    ///
    /// Storage is allocated once and then maintained incrementally by
    /// [`set`](Self::set) and [`remove`](Self::remove). A zero-capacity bitmap
    /// is valid and always behaves as empty.
    pub fn new(cap: u16) -> Self {
        let layer0_len = (cap as usize).div_ceil(WORD_BITS);
        let layer1_len = layer0_len.div_ceil(WORD_BITS);

        Self {
            cap,
            layer2: 0,
            layer1: vec![0; layer1_len],
            layer0: vec![0; layer0_len],
        }
    }

    /// Returns whether `key` is currently present.
    ///
    /// Out-of-range keys are treated as absent instead of panicking, matching
    /// the mutation APIs and keeping callers free from duplicating bounds
    /// checks.
    pub fn contains(&self, key: u16) -> bool {
        if key >= self.cap {
            return false;
        }

        let key = key as usize;
        let word_index = key >> WORD_SHIFT;
        let bit_index = key & WORD_MASK;

        self.layer0[word_index] & (1u64 << bit_index) != 0
    }

    /// Marks `key` as present.
    ///
    /// Returns `true` only when the bitmap changed. Duplicate keys and
    /// out-of-range keys return `false`.
    pub fn set(&mut self, key: u16) -> bool {
        if key >= self.cap {
            return false;
        }

        let key = key as usize;

        let l0_word_index = key >> WORD_SHIFT;
        let l0_bit_index = key & WORD_MASK;

        let l1_word_index = l0_word_index >> WORD_SHIFT;
        let l1_bit_index = l0_word_index & WORD_MASK;

        let l2_bit_index = l1_word_index;

        let l0_mask = 1u64 << l0_bit_index;
        let old_l0_word = self.layer0[l0_word_index];

        // Existing keys do not change either the leaf word or its summaries.
        if old_l0_word & l0_mask != 0 {
            return false;
        }

        self.layer0[l0_word_index] = old_l0_word | l0_mask;

        // A non-empty leaf word was already represented in both summary
        // layers, so no higher-level maintenance is required.
        if old_l0_word != 0 {
            return true;
        }

        let old_l1_word = self.layer1[l1_word_index];

        self.layer1[l1_word_index] = old_l1_word | (1u64 << l1_bit_index);

        // When this is the first non-empty leaf in the mid-level word, expose
        // that mid-level word to top-level searches as well.
        if old_l1_word == 0 {
            self.layer2 |= 1u64 << l2_bit_index;
        }

        true
    }

    /// Clears `key` from the bitmap.
    ///
    /// Returns `true` only when the bitmap changed. Missing keys and
    /// out-of-range keys return `false`.
    pub fn remove(&mut self, key: u16) -> bool {
        if key >= self.cap {
            return false;
        }

        let key = key as usize;

        let l0_word_index = key >> WORD_SHIFT;
        let l0_bit_index = key & WORD_MASK;

        let l1_word_index = l0_word_index >> WORD_SHIFT;
        let l1_bit_index = l0_word_index & WORD_MASK;

        let l2_bit_index = l1_word_index;

        let l0_mask = 1u64 << l0_bit_index;

        // Missing keys do not change either the leaf word or its summaries.
        if self.layer0[l0_word_index] & l0_mask == 0 {
            return false;
        }

        self.layer0[l0_word_index] &= !l0_mask;

        // Keep the summaries intact while this leaf word still contains at
        // least one key.
        if self.layer0[l0_word_index] != 0 {
            return true;
        }

        self.layer1[l1_word_index] &= !(1u64 << l1_bit_index);

        // Remove the top-level marker only when this mid-level word no longer
        // points to any non-empty leaf word.
        if self.layer1[l1_word_index] != 0 {
            return true;
        }

        self.layer2 &= !(1u64 << l2_bit_index);

        true
    }

    /// Returns the nearest present key strictly greater than `key`.
    ///
    /// The search starts at `key + 1`, so a present `key` is not returned.
    pub fn next(&self, key: u16) -> Option<u16> {
        let start = key as usize + 1;

        self.next_from(start)
    }

    /// Returns the nearest present key greater than or equal to `key`.
    ///
    /// The search starts at `key`, so a present `key` is returned.
    pub fn next_or_eq(&self, key: u16) -> Option<u16> {
        self.next_from(key as usize)
    }

    /// Returns the nearest present key strictly less than `key`.
    ///
    /// The search clamps to the bitmap capacity, so calling this with a key
    /// above the configured range returns the largest present key if one exists.
    pub fn prev(&self, key: u16) -> Option<u16> {
        if key == 0 || self.cap == 0 {
            return None;
        }

        let last_valid_key = self.cap as usize - 1;
        let start = (key as usize - 1).min(last_valid_key);

        self.prev_from(start)
    }

    /// Returns the nearest present key less than or equal to `key`.
    ///
    /// The search clamps to the bitmap capacity, so calling this with a key
    /// above the configured range returns the largest present key if one exists.
    pub fn prev_or_eq(&self, key: u16) -> Option<u16> {
        if self.cap == 0 {
            return None;
        }

        let last_valid_key = self.cap as usize - 1;
        let start = (key as usize).min(last_valid_key);

        self.prev_from(start)
    }

    /// Searches forward from an inclusive start index.
    ///
    /// The lookup first checks the current leaf word, then the current
    /// mid-level word, and finally the top-level summary. Each step skips an
    /// increasingly larger empty region.
    fn next_from(&self, start: usize) -> Option<u16> {
        if start >= self.cap as usize {
            return None;
        }

        let l0_word_index = start >> WORD_SHIFT;
        let l0_bit_index = start & WORD_MASK;

        let candidates = self.layer0[l0_word_index] & bits_from(l0_bit_index);

        // Fast path: the next key is in the same leaf word as `start`.
        if candidates != 0 {
            let found_bit = candidates.trailing_zeros() as usize;

            return self.make_key(l0_word_index, found_bit);
        }

        let l1_word_index = l0_word_index >> WORD_SHIFT;
        let l1_bit_index = l0_word_index & WORD_MASK;

        let candidates = self.layer1[l1_word_index] & bits_after(l1_bit_index);

        // The current leaf word is exhausted, but another non-empty leaf word
        // exists inside the same mid-level word.
        if candidates != 0 {
            let next_l0_bit = candidates.trailing_zeros() as usize;

            let next_l0_word_index = (l1_word_index << WORD_SHIFT) | next_l0_bit;

            let leaf_word = self.layer0[next_l0_word_index];

            return self.make_key(next_l0_word_index, leaf_word.trailing_zeros() as usize);
        }

        let candidates = self.layer2 & bits_after(l1_word_index);

        // No later mid-level word contains any initialized leaf word.
        if candidates == 0 {
            return None;
        }

        let next_l1_word_index = candidates.trailing_zeros() as usize;

        let next_l1_word = self.layer1[next_l1_word_index];

        let next_l0_bit = next_l1_word.trailing_zeros() as usize;

        let next_l0_word_index = (next_l1_word_index << WORD_SHIFT) | next_l0_bit;

        let leaf_word = self.layer0[next_l0_word_index];

        self.make_key(next_l0_word_index, leaf_word.trailing_zeros() as usize)
    }

    /// Searches backward from an inclusive start index.
    ///
    /// This mirrors [`next_from`](Self::next_from), using the most-significant
    /// set bit in each candidate word to locate the closest lower key.
    fn prev_from(&self, start: usize) -> Option<u16> {
        if start >= self.cap as usize {
            return None;
        }

        let l0_word_index = start >> WORD_SHIFT;
        let l0_bit_index = start & WORD_MASK;

        let candidates = self.layer0[l0_word_index] & bits_through(l0_bit_index);

        // Fast path: the previous key is in the same leaf word as `start`.
        if candidates != 0 {
            let found_bit = highest_set_bit(candidates);

            return self.make_key(l0_word_index, found_bit);
        }

        let l1_word_index = l0_word_index >> WORD_SHIFT;
        let l1_bit_index = l0_word_index & WORD_MASK;

        let candidates = self.layer1[l1_word_index] & bits_before(l1_bit_index);

        // The current leaf word is exhausted, but another non-empty leaf word
        // exists inside the same mid-level word.
        if candidates != 0 {
            let prev_l0_bit = highest_set_bit(candidates);

            let prev_l0_word_index = (l1_word_index << WORD_SHIFT) | prev_l0_bit;

            let leaf_word = self.layer0[prev_l0_word_index];

            return self.make_key(prev_l0_word_index, highest_set_bit(leaf_word));
        }

        let candidates = self.layer2 & bits_before(l1_word_index);

        // No earlier mid-level word contains any initialized leaf word.
        if candidates == 0 {
            return None;
        }

        let prev_l1_word_index = highest_set_bit(candidates);
        let prev_l1_word = self.layer1[prev_l1_word_index];

        let prev_l0_bit = highest_set_bit(prev_l1_word);

        let prev_l0_word_index = (prev_l1_word_index << WORD_SHIFT) | prev_l0_bit;

        let leaf_word = self.layer0[prev_l0_word_index];

        self.make_key(prev_l0_word_index, highest_set_bit(leaf_word))
    }

    /// Converts a leaf-word position back to a public key.
    ///
    /// The final bounds check hides padding bits in the last leaf word when
    /// `cap` is not a multiple of 64.
    fn make_key(&self, l0_word_index: usize, l0_bit_index: usize) -> Option<u16> {
        let key = (l0_word_index << WORD_SHIFT) | l0_bit_index;

        if key < self.cap as usize {
            Some(key as u16)
        } else {
            None
        }
    }
}

/// Returns a mask for bit positions greater than or equal to `bit`.
///
/// Example: `bit = 5` keeps positions `5..=63`.
#[inline]
fn bits_from(bit: usize) -> u64 {
    u64::MAX << bit
}

/// Returns a mask for bit positions strictly greater than `bit`.
///
/// Example: `bit = 5` keeps positions `6..=63`.
#[inline]
fn bits_after(bit: usize) -> u64 {
    if bit == WORD_MASK {
        0
    } else {
        u64::MAX << (bit + 1)
    }
}

/// Returns a mask for bit positions less than or equal to `bit`.
///
/// Example: `bit = 5` keeps positions `0..=5`.
#[inline]
fn bits_through(bit: usize) -> u64 {
    u64::MAX >> (WORD_MASK - bit)
}

/// Returns a mask for bit positions strictly less than `bit`.
///
/// Example: `bit = 5` keeps positions `0..=4`.
#[inline]
fn bits_before(bit: usize) -> u64 {
    if bit == 0 { 0 } else { (1u64 << bit) - 1 }
}

/// Returns the index of the most-significant set bit in a non-zero word.
#[inline]
fn highest_set_bit(value: u64) -> usize {
    WORD_MASK - value.leading_zeros() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::ops::Bound::{Excluded, Unbounded};

    fn expected_next(model: &BTreeSet<u16>, key: u16) -> Option<u16> {
        model.range((Excluded(key), Unbounded)).next().copied()
    }

    fn expected_next_or_eq(model: &BTreeSet<u16>, key: u16) -> Option<u16> {
        model.range(key..).next().copied()
    }

    fn expected_prev(model: &BTreeSet<u16>, key: u16) -> Option<u16> {
        model.range(..key).next_back().copied()
    }

    fn expected_prev_or_eq(model: &BTreeSet<u16>, key: u16) -> Option<u16> {
        model.range(..=key).next_back().copied()
    }

    fn assert_matches_model(bitmap: &HierBitmap, model: &BTreeSet<u16>, probes: &[u16]) {
        for key in 0..bitmap.cap {
            assert_eq!(
                bitmap.contains(key),
                model.contains(&key),
                "contains({key}) diverged from model"
            );
        }

        for &key in probes {
            assert_eq!(
                bitmap.contains(key),
                model.contains(&key),
                "contains({key}) diverged from model"
            );
            assert_eq!(
                bitmap.next(key),
                expected_next(model, key),
                "next({key}) diverged from model"
            );
            assert_eq!(
                bitmap.next_or_eq(key),
                expected_next_or_eq(model, key),
                "next_or_eq({key}) diverged from model"
            );
            assert_eq!(
                bitmap.prev(key),
                expected_prev(model, key),
                "prev({key}) diverged from model"
            );
            assert_eq!(
                bitmap.prev_or_eq(key),
                expected_prev_or_eq(model, key),
                "prev_or_eq({key}) diverged from model"
            );
        }
    }

    fn assert_summary_layers_are_consistent(bitmap: &HierBitmap) {
        let mut expected_layer1 = vec![0; bitmap.layer1.len()];

        for (l0_word_index, &leaf_word) in bitmap.layer0.iter().enumerate() {
            if leaf_word == 0 {
                continue;
            }

            let l1_word_index = l0_word_index >> WORD_SHIFT;
            let l1_bit_index = l0_word_index & WORD_MASK;

            expected_layer1[l1_word_index] |= 1u64 << l1_bit_index;
        }

        assert_eq!(bitmap.layer1, expected_layer1);

        let mut expected_layer2 = 0;

        for (l1_word_index, &mid_word) in bitmap.layer1.iter().enumerate() {
            if mid_word != 0 {
                expected_layer2 |= 1u64 << l1_word_index;
            }
        }

        assert_eq!(bitmap.layer2, expected_layer2);
    }

    #[test]
    fn zero_capacity_bitmap_is_a_total_noop() {
        let mut bitmap = HierBitmap::new(0);

        assert_eq!(bitmap.layer0.len(), 0);
        assert_eq!(bitmap.layer1.len(), 0);
        assert_eq!(bitmap.layer2, 0);

        assert!(!bitmap.contains(0));
        assert!(!bitmap.set(0));
        assert!(!bitmap.remove(0));
        assert_eq!(bitmap.next(0), None);
        assert_eq!(bitmap.next_or_eq(0), None);
        assert_eq!(bitmap.prev(0), None);
        assert_eq!(bitmap.prev_or_eq(0), None);
        assert_eq!(bitmap.prev(u16::MAX), None);
        assert_eq!(bitmap.prev_or_eq(u16::MAX), None);

        assert_summary_layers_are_consistent(&bitmap);
    }

    #[test]
    fn searches_cross_leaf_word_boundaries_without_returning_the_start_key() {
        let mut bitmap = HierBitmap::new(130);

        for key in [0, 63, 64, 65, 127, 128, 129] {
            assert!(bitmap.set(key));
        }

        assert_eq!(bitmap.next(0), Some(63));
        assert_eq!(bitmap.next(63), Some(64));
        assert_eq!(bitmap.next(64), Some(65));
        assert_eq!(bitmap.next(65), Some(127));
        assert_eq!(bitmap.next(127), Some(128));
        assert_eq!(bitmap.next(128), Some(129));
        assert_eq!(bitmap.next(129), None);
        assert_eq!(bitmap.next_or_eq(0), Some(0));
        assert_eq!(bitmap.next_or_eq(63), Some(63));
        assert_eq!(bitmap.next_or_eq(66), Some(127));

        assert_eq!(bitmap.prev(129), Some(128));
        assert_eq!(bitmap.prev(128), Some(127));
        assert_eq!(bitmap.prev(127), Some(65));
        assert_eq!(bitmap.prev(65), Some(64));
        assert_eq!(bitmap.prev(64), Some(63));
        assert_eq!(bitmap.prev(63), Some(0));
        assert_eq!(bitmap.prev(0), None);
        assert_eq!(bitmap.prev_or_eq(129), Some(129));
        assert_eq!(bitmap.prev_or_eq(63), Some(63));
        assert_eq!(bitmap.prev_or_eq(126), Some(65));

        assert_summary_layers_are_consistent(&bitmap);
    }

    #[test]
    fn removing_last_key_from_a_leaf_updates_only_the_required_summaries() {
        let mut bitmap = HierBitmap::new(4_200);

        assert!(bitmap.set(1));
        assert!(bitmap.set(4_095));
        assert!(bitmap.set(4_096));
        assert!(bitmap.set(4_159));

        assert_eq!(bitmap.next(1), Some(4_095));
        assert_eq!(bitmap.next(4_095), Some(4_096));
        assert_eq!(bitmap.prev(4_096), Some(4_095));

        assert!(bitmap.remove(4_096));

        assert_eq!(bitmap.next(4_095), Some(4_159));
        assert_eq!(bitmap.prev(4_159), Some(4_095));
        assert_summary_layers_are_consistent(&bitmap);

        assert!(bitmap.remove(4_159));

        assert_eq!(bitmap.next(4_095), None);
        assert_eq!(bitmap.prev(u16::MAX), Some(4_095));
        assert_summary_layers_are_consistent(&bitmap);
    }

    #[test]
    fn partial_final_leaf_word_never_exposes_padding_bits() {
        let mut bitmap = HierBitmap::new(130);

        assert!(bitmap.set(129));
        assert!(!bitmap.set(130));
        assert!(!bitmap.set(u16::MAX));

        assert_eq!(bitmap.next(128), Some(129));
        assert_eq!(bitmap.next(129), None);
        assert_eq!(bitmap.next(130), None);
        assert_eq!(bitmap.next_or_eq(129), Some(129));
        assert_eq!(bitmap.next_or_eq(130), None);
        assert_eq!(bitmap.prev(130), Some(129));
        assert_eq!(bitmap.prev(u16::MAX), Some(129));
        assert_eq!(bitmap.prev_or_eq(129), Some(129));
        assert_eq!(bitmap.prev_or_eq(u16::MAX), Some(129));

        assert_summary_layers_are_consistent(&bitmap);
    }

    #[test]
    fn maximum_u16_backed_capacity_can_address_key_65534() {
        let mut bitmap = HierBitmap::new(u16::MAX);
        let max_valid_key = 65_534;

        assert!(bitmap.set(max_valid_key));
        assert!(bitmap.contains(max_valid_key));
        assert!(!bitmap.contains(u16::MAX));

        assert_eq!(bitmap.next(max_valid_key - 1), Some(max_valid_key));
        assert_eq!(bitmap.next(max_valid_key), None);
        assert_eq!(bitmap.next_or_eq(max_valid_key), Some(max_valid_key));
        assert_eq!(bitmap.next_or_eq(u16::MAX), None);

        assert_eq!(bitmap.prev(max_valid_key), None);
        assert_eq!(bitmap.prev_or_eq(max_valid_key), Some(max_valid_key));
        assert_eq!(bitmap.prev_or_eq(u16::MAX), Some(max_valid_key));

        assert_summary_layers_are_consistent(&bitmap);
    }

    #[test]
    fn deterministic_mutation_sequence_matches_ordered_set_model() {
        let cap = 8_195;
        let mut bitmap = HierBitmap::new(cap);
        let mut model = BTreeSet::new();
        let probes = [
            0,
            1,
            62,
            63,
            64,
            65,
            127,
            128,
            255,
            256,
            4_094,
            4_095,
            4_096,
            4_097,
            8_191,
            8_192,
            8_193,
            8_194,
            8_195,
            u16::MAX,
        ];

        for key in [0, 63, 64, 4_095, 4_096, 8_191, 8_192, 8_194] {
            assert_eq!(bitmap.set(key), model.insert(key));
        }

        let mut state = 0x9E37_79B9u32;

        for step in 0..2_000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let key = (state % (cap as u32 + 257)) as u16;

            if step % 3 == 0 {
                let expected = key < cap && model.remove(&key);
                assert_eq!(bitmap.remove(key), expected, "remove({key}) at step {step}");
            } else {
                let expected = key < cap && model.insert(key);
                assert_eq!(bitmap.set(key), expected, "set({key}) at step {step}");
            }

            if step % 97 == 0 {
                assert_matches_model(&bitmap, &model, &probes);
                assert_summary_layers_are_consistent(&bitmap);
            }
        }

        assert_matches_model(&bitmap, &model, &probes);
        assert_summary_layers_are_consistent(&bitmap);
    }
}
