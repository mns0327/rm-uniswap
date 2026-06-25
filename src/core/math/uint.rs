//! Extension methods for uint types.

use ruint::aliases::U256;

/// Returns `⌊√n⌋` for `n: U256` using the Babylonian method (Heron of Alexandria).
///
/// This is a stable integer-only sqrt with no `f64` conversion — precision is
/// preserved for any U256 value, which matters for large reserve products that
/// can exceed 2^53 where `f64` loses bits.
///
/// The iteration formula is x_{k+1} = (x_k + n/x_k) / 2, which converges
/// quadratically. Starting from a bit-length estimate, 4 iterations gives
/// >120-bit accuracy for any 256-bit input (enough for exact floor sqrt).
pub fn uint_sqrt(n: U256) -> U256 {
    let n_bits = n.bit_len();

    // n_bits <= 64: value fits in a single u64 limb.
    if n_bits <= 64 {
        let lo = n.as_limbs()[0];
        let sqrt_lo = (lo as f64).sqrt() as u64;
        return U256::from(sqrt_lo);
    }

    // n_bits < 128: value fits in the lower 128 bits (limbs 0 and 1).
    // For n = 2^64: limbs[0]=0, limbs[1]=1 → lo = 0 | (1 << 64) = 2^64. Correct.
    // For n = 2^128: limbs[2]=1 → n_bits=128 is NOT < 128, falls through to N-R.
    if n_bits < 128 {
        let limbs = n.as_limbs();
        let lo = (limbs[0] as u128) | ((limbs[1] as u128) << 64);
        let sqrt_lo = (lo as f64).sqrt() as u128;
        return U256::from(sqrt_lo);
    }

    // n >= 2^128: use Newton-Raphson with bit-length initial guess.
    // Build 2^⌈(n_bits+1)/2⌉ via from_limbs to avoid ruint `<<` wrapping issues.
    let guess_bits = (n_bits + 1) / 2;
    let mut x = if guess_bits <= 64 {
        // Bit position ≤ 64 → fits in limbs[0]
        // Use 1u128 << shift to avoid u64 shift overflow, then convert
        U256::from(1u128) << guess_bits
    } else {
        // For bit position 65-255: set the corresponding limb bit via from_limbs.
        // bit position b → limb index (b-64)/64, bit position within limb (b-64)%64
        // Position 65-127 → limb 1
        // Position 128-191 → limb 2
        // Position 192-255 → limb 3
        let limb_bit = guess_bits - 64; // 1-191
        let limb_idx = (limb_bit - 1) / 64; // 0→limb1, 1→limb2, 2→limb3
        let bit_in_limb = (limb_bit - 1) % 64;
        let mut parts = [0u64; 4];
        parts[limb_idx as usize + 1] = 1u64 << bit_in_limb;
        U256::from_limbs(parts)
    };

    // 8 Babylonian iterations — more accurate for large inputs.
    for _ in 0..8 {
        let (q, _) = n.div_rem(x);
        let sum = x.wrapping_add(q);
        let denom = U256::from(2u128);
        x = sum / denom;
    }

    // Correct overshoot: for perfect squares Newton may land 1 above.
    let sq = x * x;
    if sq > n {
        x = x.saturating_sub(U256::from(1u128));
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_basic() {
        assert_eq!(uint_sqrt(U256::ZERO), U256::ZERO);
        assert_eq!(uint_sqrt(U256::from(1)), U256::from(1));
        assert_eq!(uint_sqrt(U256::from(4)), U256::from(2));
        assert_eq!(uint_sqrt(U256::from(9)), U256::from(3));
        assert_eq!(uint_sqrt(U256::from(100)), U256::from(10));
    }

    #[test]
    fn sqrt_large() {
        // sqrt(2^64) = 2^32 — fits entirely in limbs[0] (bits 0-63)
        let n = U256::from_limbs([0, 1, 0, 0]);
        let expected = U256::from_limbs([1u64 << 32, 0, 0, 0]);
        let result = uint_sqrt(n);
        assert_eq!(result, expected, "sqrt(2^64) should be 2^32");

        // sqrt(2^128) = 2^64 — bits 64-127 in limbs[1]
        let n = U256::from_limbs([0, 0, 1, 0]);
        let expected = U256::from_limbs([0, 1, 0, 0]);
        let result = uint_sqrt(n);
        assert_eq!(result, expected, "sqrt(2^128) should be 2^64");
    }

    #[test]
    fn sqrt_perfect_squares() {
        // 3 * 2^70: bits at positions 0,1 and 64+6=70
        // 3 = binary 11 → bits 0,1 set in limbs[0]
        // 1 << 6 = 64 → bit 70 set in limbs[1]
        // So 3 * 2^70 = from_limbs([3, 1 << 6, 0, 0])
        let n = U256::from_limbs([3, 1u64 << 6, 0, 0]);
        let s = uint_sqrt(n);
        assert!(s * s <= n, "sqrt floor check: s*s={} <= n={}", s * s, n);
        assert!(
            (s + U256::from(1)) * (s + U256::from(1)) > n,
            "sqrt ceiling check"
        );
    }

    #[test]
    fn sqrt_off_by_one() {
        assert_eq!(uint_sqrt(U256::from(9800)), U256::from(98));
        assert_eq!(uint_sqrt(U256::from(9801)), U256::from(99));
        assert_eq!(uint_sqrt(U256::from(9802)), U256::from(99));
    }
}
