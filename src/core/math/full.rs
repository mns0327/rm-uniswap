//! # FullMath — 512-bit Precision Multiply-Divide (Optimized)
//!
//! A fully optimized port of the `FullMath` used by Uniswap V4.
//!
//! ## Core Idea
//!
//! `mul_div(a, b, d)` computes `⌊a·b / d⌋` by forming the exact 512-bit
//! product `a·b` and dividing it by `d` without ever losing precision.
//! Instead of an expensive 512-bit division we use:
//!
//! 1. A **512-bit mulmod** to shave the remainder off the product so it
//!    becomes exactly divisible by `d`.
//! 2. A bit-trick to strip trailing powers of two from `d`, leaving an odd
//!    divisor.
//! 3. A **Newton–Raphson modular inverse** (6 iterations, 7 × 256-bit
//!    multiplications total) to divide by the odd part in pure modular
//!    arithmetic.
//!
//! ## Optimization Summary
//!
//! | Point | Before | After |
//! |---|---|---|
//! | Core slow-path operation | one 512-bit division | one 512-bit mod + seven 256-bit muls |
//! | Overflow detection | after `narrow()` | early: `prod1 >= denominator` |
//! | 256-bit fast path | none | exits immediately when `prod1 == 0` |
//! | `mul_div_rounding_up` slow path | separate 512-bit `div_rem` | shared `mul_div_core` (no extra 512-bit division) |
//! | Code duplication | two independent slow paths | single `mul_div_core` kernel |

use ruint::aliases::{U256, U512};

use super::uint::{
    add_one_if, is_power_of_two, narrow, shr_u512_to_u256, shr_u512_to_u256_rounding_up, split,
    widen,
};

/// Backward-compatible name for the crate-wide compact error code.
pub type MathError = crate::Error;

/// Computes the full 512-bit product of `a * b`.
///
/// This should only be called after the cheap 256-bit `overflowing_mul`
/// fast path has failed.
#[inline(always)]
fn full_product(a: U256, b: U256) -> (U256, U256, U512) {
    let product: U512 = a.widening_mul(b);
    let (prod0, prod1) = split(product);

    (prod0, prod1, product)
}

/// Returns `d⁻¹ mod 2²⁵⁶` for an odd `d`.
///
/// The denominator must be odd. The Newton step doubles the number of correct
/// bits on every iteration, so six iterations are enough to reach 256 bits.
#[inline(always)]
fn mod_inv_u256(d: U256) -> U256 {
    debug_assert!(d & U256::ONE == U256::ONE, "mod_inv_u256: d must be odd");

    // Initial seed correct modulo 2^4.
    let mut inv = U256::from(3u64).wrapping_mul(d) ^ U256::from(2u64);

    macro_rules! newton_step {
        () => {
            inv = inv.wrapping_mul(U256::from(2u64).wrapping_sub(d.wrapping_mul(inv)));
        };
    }

    newton_step!();
    newton_step!();
    newton_step!();
    newton_step!();
    newton_step!();
    newton_step!();

    inv
}

/// Core computation for the slow path where `a * b` does not fit into 256 bits.
///
/// Returns:
///
/// - `quotient`: floor(a * b / denominator)
/// - `has_remainder`: true if the division was not exact
///
/// Important optimization:
///
/// If `denominator` is a power of two, the quotient can be computed with a
/// shift and the expensive 512-bit modulo + modular inverse path can be skipped.
#[inline(always)]
fn mul_div_core(
    prod0: U256,
    prod1: U256,
    denominator: U256,
    product: U512,
) -> Result<(U256, bool), MathError> {
    // The quotient fits into U256 iff prod1 < denominator.
    if prod1 >= denominator {
        return Err(MathError::Overflow);
    }

    // Fast slow-path shortcut:
    // If denominator is a power of two, division is just a right shift.
    if is_power_of_two(denominator) {
        let shift = denominator.trailing_zeros() as u32;

        let quotient = if shift == 0 {
            prod0
        } else {
            (prod0 >> shift) | (prod1 << (256u32 - shift))
        };

        // For a power-of-two denominator, the remainder is the masked-off
        // low bits of prod0.
        let has_remainder = !(prod0 & (denominator - U256::ONE)).is_zero();

        return Ok((quotient, has_remainder));
    }

    // Compute remainder = (a * b) % denominator.
    //
    // This is the expensive part, so we only reach this path for true 512-bit
    // products with non-power-of-two denominators.
    let remainder = narrow(product % widen(denominator))
        .expect("remainder is always smaller than denominator and must fit in U256");

    let has_remainder = !remainder.is_zero();

    // Subtract the remainder from the 512-bit product so the result becomes
    // exactly divisible by denominator.
    let (prod0, borrow) = prod0.overflowing_sub(remainder);
    let prod1 = if borrow { prod1 - U256::ONE } else { prod1 };

    // Factor powers of two out of denominator.
    let twos = denominator & denominator.wrapping_neg();
    let shift = twos.trailing_zeros() as u32;
    let odd_denom = denominator >> shift;

    // Divide the adjusted 512-bit product by the extracted power of two.
    let prod0 = if shift == 0 {
        prod0
    } else {
        (prod0 >> shift) | (prod1 << (256u32 - shift))
    };

    // Divide by the odd denominator using modular inverse.
    let quotient = prod0.wrapping_mul(mod_inv_u256(odd_denom));

    Ok((quotient, has_remainder))
}

/// Computes floor(a * b / denominator).
///
/// Main optimization:
///
/// We first try a cheap 256-bit multiplication with `overflowing_mul`.
/// If it does not overflow, we avoid creating a full U512 product entirely.
#[inline]
#[must_use = "discarding a Result from mul_div silently ignores errors"]
pub fn mul_div(a: U256, b: U256, denominator: U256) -> Result<U256, MathError> {
    if denominator.is_zero() {
        return Err(MathError::ZeroDenominator);
    }

    // Special case: division by 1.
    //
    // We only need to check whether the 256-bit multiplication overflows.
    if denominator == U256::ONE {
        let (product, overflow) = a.overflowing_mul(b);

        return if overflow {
            Err(MathError::Overflow)
        } else {
            Ok(product)
        };
    }

    // First try the cheap 256-bit path.
    //
    // The old version always built a U512 product first, even when the product
    // actually fit into 256 bits. This was wasting time on the common case.
    let (prod0, overflow) = a.overflowing_mul(b);

    if !overflow {
        return Ok(prod0 / denominator);
    }

    // Only build the full 512-bit product when the 256-bit product overflows.
    let (prod0, prod1, product) = full_product(a, b);

    let (quotient, _) = mul_div_core(prod0, prod1, denominator, product)?;

    Ok(quotient)
}

/// Computes ceil(a * b / denominator).
///
/// This shares the same optimized paths as `mul_div`, but also tracks whether
/// the division had a non-zero remainder.
#[inline]
#[must_use = "discarding a Result from mul_div_rounding_up silently ignores errors"]
pub fn mul_div_rounding_up(a: U256, b: U256, denominator: U256) -> Result<U256, MathError> {
    if denominator.is_zero() {
        return Err(MathError::ZeroDenominator);
    }

    // Special case: division by 1.
    if denominator == U256::ONE {
        let (product, overflow) = a.overflowing_mul(b);

        return if overflow {
            Err(MathError::Overflow)
        } else {
            Ok(product)
        };
    }

    // First try the cheap 256-bit path.
    let (prod0, overflow) = a.overflowing_mul(b);

    if !overflow {
        let (q, r) = prod0.div_rem(denominator);

        return add_one_if(q, !r.is_zero());
    }

    // Only build the full 512-bit product when necessary.
    let (prod0, prod1, product) = full_product(a, b);

    let (quotient, has_remainder) = mul_div_core(prod0, prod1, denominator, product)?;

    add_one_if(quotient, has_remainder)
}

#[inline(always)]
pub fn mul_shift_right(a: U256, b: U256, shift: u32) -> Result<U256, MathError> {
    debug_assert!(shift < 256);

    let product: U512 = a.widening_mul(b);
    shr_u512_to_u256(product, shift)
}

#[inline(always)]
pub fn mul_shift_right_rounding_up(a: U256, b: U256, shift: u32) -> Result<U256, MathError> {
    debug_assert!(shift < 256);

    let product: U512 = a.widening_mul(b);
    shr_u512_to_u256_rounding_up(product, shift)
}

/// Computes floor((amount * Q96) / denominator).
///
/// This is a specialized replacement for:
///
/// ```text
/// mul_div(amount, Q96, denominator)
/// ```
///
/// Since Q96 = 2^96, the numerator is:
///
/// ```text
/// amount << 96
/// ```
///
/// So we avoid a full U256 x U256 multiplication completely.
#[inline(always)]
pub fn mul_q96_div(amount: U256, denominator: U256) -> Result<U256, MathError> {
    if denominator.is_zero() {
        return Err(MathError::ZeroDenominator);
    }

    if amount.is_zero() {
        return Ok(U256::ZERO);
    }

    // denominator == 1:
    // result = amount << 96
    if denominator == U256::ONE {
        if amount > (U256::MAX >> 96) {
            return Err(MathError::Overflow);
        }

        return Ok(amount << 96);
    }

    // If denominator is a power of two, avoid division completely.
    //
    // (amount << 96) / 2^shift
    //
    // If shift <= 96:
    //     amount << (96 - shift)
    //
    // If shift > 96:
    //     amount >> (shift - 96)
    if is_power_of_two(denominator) {
        let shift = denominator.trailing_zeros() as u32;

        if shift <= 96 {
            let left_shift = 96 - shift;

            if left_shift == 0 {
                return Ok(amount);
            }

            if amount > (U256::MAX >> left_shift) {
                return Err(MathError::Overflow);
            }

            return Ok(amount << left_shift);
        }

        return Ok(amount >> (shift - 96));
    }

    // Fast path:
    // If amount << 96 fits into U256, use normal U256 division.
    //
    // This is usually much cheaper than building a U512 numerator.
    if amount <= (U256::MAX >> 96) {
        return Ok((amount << 96) / denominator);
    }

    // Slow path:
    //
    // The conceptual numerator is:
    //
    //     amount << 96
    //
    // Its high 256 bits are:
    //
    //     amount >> 160
    //
    // For floor(numerator / denominator) to fit into U256,
    // the high half must be strictly smaller than denominator.
    let numerator_high = amount >> 160;

    if numerator_high >= denominator {
        return Err(MathError::Overflow);
    }

    // Build the exact shifted numerator without multiplication.
    let numerator = widen(amount) << 96;

    let quotient = numerator / widen(denominator);

    narrow(quotient)
}

/// Computes ceil((amount * Q96) / denominator).
///
/// Specialized replacement for:
///
/// ```text
/// mul_div_rounding_up(amount, Q96, denominator)
/// ```
///
/// This avoids U256 x U256 multiplication and avoids the generic FullMath
/// modular-inverse slow path.
#[inline(always)]
pub fn mul_q96_div_rounding_up(amount: U256, denominator: U256) -> Result<U256, MathError> {
    if denominator.is_zero() {
        return Err(MathError::ZeroDenominator);
    }

    if amount.is_zero() {
        return Ok(U256::ZERO);
    }

    // denominator == 1:
    // result is exact, no rounding needed.
    if denominator == U256::ONE {
        if amount > (U256::MAX >> 96) {
            return Err(MathError::Overflow);
        }

        return Ok(amount << 96);
    }

    // Power-of-two denominator:
    //
    // This avoids division completely and computes the remainder from the
    // bits shifted out.
    if is_power_of_two(denominator) {
        let shift = denominator.trailing_zeros() as u32;

        if shift <= 96 {
            let left_shift = 96 - shift;

            if left_shift == 0 {
                return Ok(amount);
            }

            if amount > (U256::MAX >> left_shift) {
                return Err(MathError::Overflow);
            }

            return Ok(amount << left_shift);
        }

        let right_shift = shift - 96;
        let quotient = amount >> right_shift;

        let mask = (U256::ONE << right_shift) - U256::ONE;
        let has_remainder = !(amount & mask).is_zero();

        add_one_if(quotient, has_remainder)
    } else {
        // Fast path:
        // amount << 96 fits into U256.
        if amount <= (U256::MAX >> 96) {
            let shifted: U256 = amount << 96;
            let (q, r) = shifted.div_rem(denominator);

            return add_one_if(q, !r.is_zero());
        }

        // Slow path:
        //
        // The high 256 bits of amount << 96 are amount >> 160.
        // If that high half is >= denominator, the floor quotient already
        // exceeds U256::MAX.
        let numerator_high = amount >> 160;

        if numerator_high >= denominator {
            return Err(MathError::Overflow);
        }

        let numerator: U512 = widen(amount) << 96;
        let denominator_512 = widen(denominator);

        let (quotient_512, remainder_512) = numerator.div_rem(denominator_512);

        let quotient = narrow(quotient_512)?;

        add_one_if(quotient, !remainder_512.is_zero())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2¹²⁸ — a convenient large boundary value used throughout the tests.
    fn q128() -> U256 {
        U256::ONE << 128
    }

    #[test]
    fn mul_div_phantom_overflow() {
        // a·b > U256::MAX but the quotient still fits in 256 bits.
        // Exercises the full slow path: mulmod, power-of-two stripping, N-R inverse.
        let q = q128();
        let result = mul_div(q, U256::from(35) * q, U256::from(8) * q).unwrap();
        let expected = U256::from(35) * q / U256::from(8); // = 4.375 · 2¹²⁸
        assert_eq!(result, expected);
    }

    #[test]
    fn mul_div_exact() {
        // 4·q · q / (2·q) = 2·q  (exact, no rounding artefacts).
        let q = q128();
        assert_eq!(
            mul_div(U256::from(4) * q, q, U256::from(2) * q).unwrap(),
            U256::from(2) * q
        );
    }

    #[test]
    fn mul_div_zero_denominator() {
        assert_eq!(
            mul_div(U256::ONE, U256::ONE, U256::ZERO),
            Err(MathError::ZeroDenominator)
        );
    }

    #[test]
    fn mul_div_overflow() {
        // prod1 >= denominator triggers the early overflow guard.
        // U256::MAX × U256::MAX / 1: prod1 = U256::MAX-1 ≥ 1 = d → Overflow.
        assert_eq!(
            mul_div(U256::MAX, U256::MAX, U256::ONE),
            Err(MathError::Overflow)
        );
        // Second extreme: prod1 == denominator is still ≥, so it also overflows.
        // Here d = 2 and a·b = U256::MAX × U256::MAX again — same as above.
        assert_eq!(
            mul_div(U256::MAX, U256::MAX, U256::from(2u64)),
            Err(MathError::Overflow)
        );
    }

    #[test]
    fn mul_div_exact_at_max() {
        // q × U256::MAX / q = U256::MAX exactly (no overflow, quotient fits).
        //
        // Proof: q·U256::MAX = 2¹²⁸·(2²⁵⁶−1).
        //   prod0 = 2²⁵⁶ − 2¹²⁸,  prod1 = 2¹²⁸ − 1 = q − 1.
        //   prod1 = q−1 < q = denominator  → early overflow check PASSES.
        //   Quotient = (2²⁵⁶ − 2¹²⁸) / 2¹²⁸ + correction = 2¹²⁸·(2¹²⁸−1)/2¹²⁸ = 2²⁵⁶−1
        //   = U256::MAX.
        //
        // The former test incorrectly expected Err(Overflow) for this case.
        let q = q128();
        assert_eq!(mul_div(q, U256::MAX, q).unwrap(), U256::MAX);
    }

    #[test]
    fn mul_div_fast_path() {
        // prod1 == 0: the fast path (plain 256-bit division) is taken.
        assert_eq!(
            mul_div(U256::from(10u64), U256::from(3u64), U256::from(5u64)).unwrap(),
            U256::from(6u64)
        );
    }

    #[test]
    fn rounding_up_with_remainder() {
        // ⌈q128 / 3⌉ = ⌊q128 / 3⌋ + 1  (q128 is not divisible by 3).
        let q = q128();
        let result = mul_div_rounding_up(q, U256::from(1000) * q, U256::from(3000) * q).unwrap();
        assert_eq!(result, q / U256::from(3) + U256::ONE);
    }

    #[test]
    fn rounding_up_exact() {
        // ⌈4·q / 2⌉ = 2·q  (zero remainder; ceiling equals floor).
        let q = q128();
        assert_eq!(
            mul_div_rounding_up(U256::from(4) * q, q, U256::from(2) * q).unwrap(),
            U256::from(2) * q
        );
    }

    #[test]
    fn rounding_up_overflow_at_max() {
        // Construct a case where ⌊a·b/d⌋ = U256::MAX with a non-zero remainder,
        // so ⌈a·b/d⌉ = U256::MAX + 1 — which must return Err(Overflow).
        //
        // Let x = U256::MAX.  By polynomial long division:
        //
        //     (x−1)² = (x−2)·x + 1
        //
        // Therefore:  ⌊(x−1)² / (x−2)⌋ = x = U256::MAX,  remainder = 1 ≠ 0.
        //
        // Numeric check on prod1:
        //   (x−1)² in 512 bits: (2²⁵⁶−2)² = (2²⁵⁶−4)·2²⁵⁶ + 4
        //   → prod0 = 4,  prod1 = 2²⁵⁶−4 = x−3.
        //   denominator = x−2,  and  x−3 < x−2  ✓  (early overflow check passes).
        //
        // WHY THE OLD TEST WAS WRONG:
        //   mul_div_rounding_up(U256::MAX, 2, 2) was expected to return Err(Overflow),
        //   but U256::MAX × 2 / 2 = U256::MAX exactly (remainder = 0), so the
        //   correct answer is Ok(U256::MAX).
        let x = U256::MAX;
        assert_eq!(
            mul_div_rounding_up(x - U256::ONE, x - U256::ONE, x - U256::from(2u64)),
            Err(MathError::Overflow)
        );

        // Confirm that the corresponding floor (mul_div) does NOT overflow,
        // and correctly yields U256::MAX.
        assert_eq!(
            mul_div(x - U256::ONE, x - U256::ONE, x - U256::from(2u64)).unwrap(),
            U256::MAX
        );

        // Exact division must not round up or overflow.
        assert_eq!(
            mul_div_rounding_up(U256::MAX, U256::from(2u64), U256::from(2u64)).unwrap(),
            U256::MAX // exact: remainder = 0, no increment needed
        );
    }

    #[test]
    fn rounding_up_fast_path() {
        // ⌈7/2⌉ = 4; exercises the 256-bit fast path inside mul_div_rounding_up.
        assert_eq!(
            mul_div_rounding_up(U256::from(7u64), U256::from(1u64), U256::from(2u64)).unwrap(),
            U256::from(4u64)
        );
    }

    #[test]
    fn mod_inv_correctness() {
        // d · d⁻¹ ≡ 1 (mod 2²⁵⁶) for a representative set of odd values.
        for &d in &[1u64, 3, 5, 7, 13, 101, u64::MAX] {
            let d256 = U256::from(d);
            let inv = mod_inv_u256(d256);
            assert_eq!(
                d256.wrapping_mul(inv),
                U256::ONE,
                "mod_inv failed for d = {d}"
            );
        }
    }

    #[test]
    fn mod_inv_large_odd() {
        // 2²⁵⁵ − 1 is odd; verify d · d⁻¹ ≡ 1 (mod 2²⁵⁶) for a near-max value.
        let d = (U256::ONE << 255) - U256::ONE;
        let inv = mod_inv_u256(d);
        assert_eq!(d.wrapping_mul(inv), U256::ONE);
    }

    #[test]
    fn slow_path_ceil_equals_floor_or_floor_plus_one() {
        // For every case: ⌈a·b/d⌉ ∈ { ⌊a·b/d⌋,  ⌊a·b/d⌋ + 1 }.
        // Exercises the shared mul_div_core slow path for both functions.
        let q = q128();

        let cases: &[(U256, U256, U256)] = &[
            // 512-bit products with non-trivial remainders.
            (U256::MAX, U256::from(3u64), U256::from(7u64)),
            (q, q * U256::from(5), q * U256::from(3)),
            // Denominator with trailing zeros (tests power-of-two factoring).
            (U256::MAX - U256::ONE, U256::from(2u64), U256::from(4u64)),
            // Denominator is a power of two (odd_denom == 1 after shifting).
            (U256::MAX, U256::from(1u64), U256::from(256u64)),
        ];

        for &(a, b, d) in cases {
            if d.is_zero() {
                continue;
            }
            // Skip cases where the floor itself overflows (both functions agree).
            let floor = match mul_div(a, b, d) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match mul_div_rounding_up(a, b, d) {
                Ok(ceil) => {
                    assert!(
                        ceil == floor || ceil == floor + U256::ONE,
                        "a={a}, b={b}, d={d}: ceil={ceil} floor={floor}"
                    );
                }
                // Overflow on the ceiling is acceptable when floor == U256::MAX.
                Err(MathError::Overflow) => {
                    assert_eq!(
                        floor,
                        U256::MAX,
                        "ceiling overflowed but floor != U256::MAX (a={a}, b={b}, d={d})"
                    );
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
    }

    #[test]
    fn rounding_up_slow_path_exact_no_increment() {
        // When the slow path produces an exact result, mul_div_rounding_up
        // must equal mul_div (has_remainder == false, no +1 applied).
        let q = q128();
        // 4·q · q / (2·q) = 2·q exactly, even through the slow path.
        // Force the slow path: a·b = 4·q² > U256::MAX when q = 2¹²⁸.
        let a = U256::from(4) * q;
        let b = q;
        let d = U256::from(2) * q;
        assert_eq!(
            mul_div(a, b, d).unwrap(),
            mul_div_rounding_up(a, b, d).unwrap()
        );
    }
}
