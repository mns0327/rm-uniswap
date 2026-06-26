//! Exact arithmetic for `U256 × u32 / u32`.
//!
//! Swap fees and protocol-fee shares repeatedly multiply a token amount by a
//! small pips-style numerator and divide by a small denominator. Routing those
//! cases through generic 512-bit `FullMath` works, but costs more than needed
//! and invites subtle drift when light, full, and cached swap paths each carry
//! their own copy. This module owns that one primitive.

use ruint::aliases::U256;

use crate::Error as MathError;

use super::uint::add_one_if;

/// Exact helper for:
///
/// ```text
/// floor(a * numerator / denominator)
/// ```
///
/// where `numerator` and `denominator` are small `u32` values.
#[inline(always)]
pub(crate) fn mul_div_u32_floor(
    amount: U256,
    numerator: u32,
    denominator: u32,
) -> Result<U256, MathError> {
    let (quotient, _) = mul_div_u32_div_rem(amount, numerator, denominator)?;
    Ok(quotient)
}

/// Exact helper for:
///
/// ```text
/// ceil(a * numerator / denominator)
/// ```
///
/// where `numerator` and `denominator` are small `u32` values.
#[inline(always)]
pub(crate) fn mul_div_u32_ceil(
    amount: U256,
    numerator: u32,
    denominator: u32,
) -> Result<U256, MathError> {
    let (quotient, remainder) = mul_div_u32_div_rem(amount, numerator, denominator)?;
    add_one_if(quotient, remainder != 0)
}

/// Exact `U256 × u32 / u32` long division.
///
/// Returns `(quotient, remainder)`. The intermediate product is represented as
/// five 64-bit limbs because `U256 * u32` can require up to 288 bits.
#[inline(always)]
pub(crate) fn mul_div_u32_div_rem(
    amount: U256,
    numerator: u32,
    denominator: u32,
) -> Result<(U256, u32), MathError> {
    if denominator == 0 {
        return Err(MathError::ZeroDenominator);
    }

    if amount.is_zero() || numerator == 0 {
        return Ok((U256::ZERO, 0));
    }

    if numerator == denominator {
        return Ok((amount, 0));
    }

    let numerator = numerator as u128;
    let denominator = denominator as u128;
    let [a0, a1, a2, a3] = amount.into_limbs();

    let mut product = [0u64; 5];
    let mut carry = 0u128;

    let t0 = (a0 as u128) * numerator + carry;
    product[0] = t0 as u64;
    carry = t0 >> 64;

    let t1 = (a1 as u128) * numerator + carry;
    product[1] = t1 as u64;
    carry = t1 >> 64;

    let t2 = (a2 as u128) * numerator + carry;
    product[2] = t2 as u64;
    carry = t2 >> 64;

    let t3 = (a3 as u128) * numerator + carry;
    product[3] = t3 as u64;
    carry = t3 >> 64;

    product[4] = carry as u64;

    let mut quotient = [0u64; 5];
    let mut remainder = 0u128;

    let cur4 = (remainder << 64) | product[4] as u128;
    quotient[4] = (cur4 / denominator) as u64;
    remainder = cur4 % denominator;

    let cur3 = (remainder << 64) | product[3] as u128;
    quotient[3] = (cur3 / denominator) as u64;
    remainder = cur3 % denominator;

    let cur2 = (remainder << 64) | product[2] as u128;
    quotient[2] = (cur2 / denominator) as u64;
    remainder = cur2 % denominator;

    let cur1 = (remainder << 64) | product[1] as u128;
    quotient[1] = (cur1 / denominator) as u64;
    remainder = cur1 % denominator;

    let cur0 = (remainder << 64) | product[0] as u128;
    quotient[0] = (cur0 / denominator) as u64;
    remainder = cur0 % denominator;

    if quotient[4] != 0 {
        return Err(MathError::Overflow);
    }

    Ok((
        U256::from_limbs([quotient[0], quotient[1], quotient[2], quotient[3]]),
        remainder as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_and_ceil_match_expected_small_values() {
        assert_eq!(
            mul_div_u32_floor(U256::from(10u64), 3, 4).unwrap(),
            U256::from(7u64)
        );
        assert_eq!(
            mul_div_u32_ceil(U256::from(10u64), 3, 4).unwrap(),
            U256::from(8u64)
        );
    }

    #[test]
    fn exact_result_does_not_round_up() {
        assert_eq!(
            mul_div_u32_ceil(U256::from(10u64), 2, 5).unwrap(),
            U256::from(4u64)
        );
    }

    #[test]
    fn zero_denominator_is_rejected() {
        assert_eq!(
            mul_div_u32_floor(U256::ONE, 1, 0),
            Err(MathError::ZeroDenominator)
        );
    }

    #[test]
    fn detects_quotient_overflow() {
        assert_eq!(mul_div_u32_floor(U256::MAX, 2, 1), Err(MathError::Overflow));
    }
}
