//! Shared fixed-width integer helpers for the math layer.
//!
//! The concentrated-liquidity math modules all need the same small set of
//! operations around `U256`/`U512`: widen, narrow, split, checked round-up, and
//! exact division with optional ceiling semantics. Keeping those primitives here
//! makes the higher-level formulas easier to audit against Solidity parity.

use ruint::aliases::{U256, U512};

use crate::Error as MathError;

/// Returns true when `x` is a non-zero power of two.
#[inline(always)]
pub(crate) fn is_power_of_two(x: U256) -> bool {
    !x.is_zero() && (x & (x - U256::ONE)).is_zero()
}

/// Zero-extends a [`U256`] into the low half of a [`U512`].
#[inline(always)]
pub(crate) fn widen(v: U256) -> U512 {
    let [a, b, c, d] = v.into_limbs();
    U512::from_limbs([a, b, c, d, 0, 0, 0, 0])
}

/// Splits a [`U512`] into `(low: U256, high: U256)`.
#[inline(always)]
pub(crate) fn split(v: U512) -> (U256, U256) {
    let [a, b, c, d, e, f, g, h] = v.into_limbs();

    (
        U256::from_limbs([a, b, c, d]),
        U256::from_limbs([e, f, g, h]),
    )
}

/// Narrows a [`U512`] into [`U256`].
///
/// Returns [`MathError::Overflow`] if the high 256 bits are non-zero.
#[inline(always)]
pub(crate) fn narrow(v: U512) -> Result<U256, MathError> {
    let [a, b, c, d, e, f, g, h] = v.into_limbs();

    if (e | f | g | h) != 0 {
        return Err(MathError::Overflow);
    }

    Ok(U256::from_limbs([a, b, c, d]))
}

/// Adds one to `value` when `has_remainder` is true.
#[inline(always)]
pub(crate) fn add_one_if(value: U256, has_remainder: bool) -> Result<U256, MathError> {
    if has_remainder {
        value.checked_add(U256::ONE).ok_or(MathError::Overflow)
    } else {
        Ok(value)
    }
}

/// Computes `ceil(numerator / denominator)` for non-zero `U256` denominator.
#[inline(always)]
pub(crate) fn div_rounding_up_u256_nonzero(
    numerator: U256,
    denominator: U256,
) -> Result<U256, MathError> {
    debug_assert!(!denominator.is_zero());

    let (quotient, remainder) = numerator.div_rem(denominator);
    add_one_if(quotient, !remainder.is_zero())
}

/// Computes `floor(numerator / denominator)` where both are represented wide.
#[inline(always)]
pub(crate) fn div_u512_by_u512_floor(
    numerator: U512,
    denominator: U512,
) -> Result<U256, MathError> {
    debug_assert!(!denominator.is_zero());
    narrow(numerator / denominator)
}

/// Computes `ceil(numerator / denominator)` where both are represented wide.
#[inline(always)]
pub(crate) fn div_u512_by_u512_rounding_up(
    numerator: U512,
    denominator: U512,
) -> Result<U256, MathError> {
    debug_assert!(!denominator.is_zero());

    let (quotient, remainder) = numerator.div_rem(denominator);
    add_one_if(narrow(quotient)?, !remainder.is_zero())
}

/// Computes `ceil(numerator / denominator)` for a wide numerator and U256 denominator.
#[inline(always)]
pub(crate) fn div_u512_by_u256_rounding_up(
    numerator: U512,
    denominator: U256,
) -> Result<U256, MathError> {
    debug_assert!(!denominator.is_zero());
    div_u512_by_u512_rounding_up(numerator, widen(denominator))
}

/// Shifts a wide product right by `shift` bits and narrows the result.
///
/// `shift` must be less than 256. This matches the Q-format use cases in this
/// crate where the discarded bits always live in the low half.
#[inline(always)]
pub(crate) fn shr_u512_to_u256(product: U512, shift: u32) -> Result<U256, MathError> {
    debug_assert!(shift < 256);

    let (low, high) = split(product);

    if shift == 0 {
        return if high.is_zero() {
            Ok(low)
        } else {
            Err(MathError::Overflow)
        };
    }

    if (high >> shift) != U256::ZERO {
        return Err(MathError::Overflow);
    }

    Ok((low >> shift) | (high << (256 - shift)))
}

/// Shifts a wide product right by `shift` bits, rounding up on discarded bits.
#[inline(always)]
pub(crate) fn shr_u512_to_u256_rounding_up(product: U512, shift: u32) -> Result<U256, MathError> {
    debug_assert!(shift < 256);

    let (low, _) = split(product);
    let quotient = shr_u512_to_u256(product, shift)?;
    let mask = (U256::ONE << shift) - U256::ONE;
    let has_remainder = !(low & mask).is_zero();

    add_one_if(quotient, has_remainder)
}
