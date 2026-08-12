//! Exact `U256 * u32 / u32` arithmetic for Uniswap fee ratios.

use ruint::aliases::U256;

use super::full::MathError;

#[inline(always)]
fn div_step_u32(remainder: u64, limb: u64, denominator: u32) -> (u64, u64) {
    debug_assert!(denominator != 0);
    debug_assert!(remainder < denominator as u64);

    let denominator = denominator as u128;
    let current = ((remainder as u128) << 64) | limb as u128;
    let quotient = current / denominator;
    let remainder = current - quotient * denominator;

    debug_assert!(quotient <= u64::MAX as u128);
    debug_assert!(remainder < denominator);

    (quotient as u64, remainder as u64)
}

#[inline(always)]
fn divide_leading_u32(leading: u64, denominator: u32) -> (u64, u64) {
    let denominator = denominator as u64;

    if leading < denominator {
        (0, leading)
    } else {
        let quotient = leading / denominator;
        let remainder = leading - quotient * denominator;
        (quotient, remainder)
    }
}

#[inline(always)]
fn mul_div_low64_u32(a0: u64, numerator: u32, denominator: u32) -> (U256, u32) {
    let product = (a0 as u128) * (numerator as u128);
    let p0 = product as u64;
    let p1 = (product >> 64) as u64;

    let (q1, remainder) = divide_leading_u32(p1, denominator);
    let (q0, remainder) = div_step_u32(remainder, p0, denominator);

    (U256::from_limbs([q0, q1, 0, 0]), remainder as u32)
}

#[inline(always)]
fn mul_div_low128_u32(a0: u64, a1: u64, numerator: u32, denominator: u32) -> (U256, u32) {
    let numerator = numerator as u128;

    let product0 = (a0 as u128) * numerator;
    let p0 = product0 as u64;
    let carry0 = product0 >> 64;

    let product1 = (a1 as u128) * numerator + carry0;
    let p1 = product1 as u64;
    let p2 = (product1 >> 64) as u64;

    let (q2, remainder) = divide_leading_u32(p2, denominator);
    let (q1, remainder) = div_step_u32(remainder, p1, denominator);
    let (q0, remainder) = div_step_u32(remainder, p0, denominator);

    (U256::from_limbs([q0, q1, q2, 0]), remainder as u32)
}

#[inline(always)]
fn mul_div_full_u32(
    amount: U256,
    numerator: u32,
    denominator: u32,
) -> Result<(U256, u32), MathError> {
    let [a0, a1, a2, a3] = amount.into_limbs();
    let numerator = numerator as u128;

    let product0 = (a0 as u128) * numerator;
    let p0 = product0 as u64;
    let carry0 = product0 >> 64;

    let product1 = (a1 as u128) * numerator + carry0;
    let p1 = product1 as u64;
    let carry1 = product1 >> 64;

    let product2 = (a2 as u128) * numerator + carry1;
    let p2 = product2 as u64;
    let carry2 = product2 >> 64;

    let product3 = (a3 as u128) * numerator + carry2;
    let p3 = product3 as u64;
    let p4 = (product3 >> 64) as u64;

    let denominator64 = denominator as u64;
    if p4 >= denominator64 {
        return Err(MathError::Overflow);
    }

    let (q3, remainder) = div_step_u32(p4, p3, denominator);
    let (q2, remainder) = div_step_u32(remainder, p2, denominator);
    let (q1, remainder) = div_step_u32(remainder, p1, denominator);
    let (q0, remainder) = div_step_u32(remainder, p0, denominator);

    Ok((U256::from_limbs([q0, q1, q2, q3]), remainder as u32))
}

#[inline(always)]
fn mul_div_u32_div_rem(
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

    let [a0, a1, a2, a3] = amount.into_limbs();

    if (a1 | a2 | a3) == 0 {
        return Ok(mul_div_low64_u32(a0, numerator, denominator));
    }

    if (a2 | a3) == 0 {
        return Ok(mul_div_low128_u32(a0, a1, numerator, denominator));
    }

    mul_div_full_u32(amount, numerator, denominator)
}

#[inline(always)]
#[must_use = "discarding a Result from mul_div_u32_floor silently ignores errors"]
pub(crate) fn mul_div_u32_floor(
    amount: U256,
    numerator: u32,
    denominator: u32,
) -> Result<U256, MathError> {
    let (quotient, _) = mul_div_u32_div_rem(amount, numerator, denominator)?;
    Ok(quotient)
}

#[inline(always)]
#[must_use = "discarding a Result from mul_div_u32_ceil silently ignores errors"]
pub(crate) fn mul_div_u32_ceil(
    amount: U256,
    numerator: u32,
    denominator: u32,
) -> Result<U256, MathError> {
    let (quotient, remainder) = mul_div_u32_div_rem(amount, numerator, denominator)?;

    if remainder == 0 {
        Ok(quotient)
    } else {
        quotient.checked_add(U256::ONE).ok_or(MathError::Overflow)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ruint::aliases::U512;

    use super::*;

    #[allow(dead_code)]
    fn widen(value: U256) -> U512 {
        let [a0, a1, a2, a3] = value.into_limbs();
        U512::from_limbs([a0, a1, a2, a3, 0, 0, 0, 0])
    }

    #[allow(dead_code)]
    fn narrow(value: U512) -> U256 {
        let [a0, a1, a2, a3, ..] = value.into_limbs();
        U256::from_limbs([a0, a1, a2, a3])
    }

    #[allow(dead_code)]
    fn reference_floor(amount: U256, numerator: u32, denominator: u32) -> Result<U256, MathError> {
        if denominator == 0 {
            return Err(MathError::ZeroDenominator);
        }

        let numerator = widen(amount) * U512::from(numerator);
        let quotient = numerator / U512::from(denominator);

        if quotient > widen(U256::MAX) {
            return Err(MathError::Overflow);
        }

        Ok(narrow(quotient))
    }

    #[allow(dead_code)]
    fn reference_ceil(amount: U256, numerator: u32, denominator: u32) -> Result<U256, MathError> {
        if denominator == 0 {
            return Err(MathError::ZeroDenominator);
        }

        let numerator = widen(amount) * U512::from(numerator);
        let denominator = U512::from(denominator);
        let quotient = numerator / denominator;
        let remainder = numerator % denominator;

        let rounded = if remainder.is_zero() {
            quotient
        } else {
            quotient + U512::ONE
        };

        if rounded > widen(U256::MAX) {
            return Err(MathError::Overflow);
        }

        Ok(narrow(rounded))
    }

    #[allow(dead_code)]
    fn arb_u256() -> impl Strategy<Value = U256> {
        any::<[u64; 4]>().prop_map(U256::from_limbs)
    }

    #[test]
    fn zero_denominator_returns_error() {
        assert_eq!(
            mul_div_u32_floor(U256::ONE, 1, 0),
            Err(MathError::ZeroDenominator)
        );
        assert_eq!(
            mul_div_u32_ceil(U256::ONE, 1, 0),
            Err(MathError::ZeroDenominator)
        );
    }

    #[test]
    fn trivial_cases() {
        assert_eq!(
            mul_div_u32_floor(U256::from(123u64), 0, 7).unwrap(),
            U256::ZERO
        );
        assert_eq!(mul_div_u32_ceil(U256::ZERO, 3, 7).unwrap(), U256::ZERO);
        assert_eq!(
            mul_div_u32_floor(U256::from(123u64), 9, 9).unwrap(),
            U256::from(123u64)
        );
    }

    #[test]
    fn ceil_rounds_up_only_with_remainder() {
        assert_eq!(
            mul_div_u32_ceil(U256::from(10u64), 3, 5).unwrap(),
            U256::from(6u64)
        );
        assert_eq!(
            mul_div_u32_ceil(U256::from(10u64), 3, 4).unwrap(),
            U256::from(8u64)
        );
    }

    #[test]
    fn full_width_floor_can_reach_max() {
        assert_eq!(
            mul_div_u32_floor(U256::MAX, 1_000_000, 1_000_000).unwrap(),
            U256::MAX
        );
    }

    #[test]
    fn ceil_increment_can_overflow() {
        assert_eq!(
            mul_div_u32_ceil(U256::MAX, 1_000_000, 999_999),
            Err(MathError::Overflow)
        );
    }

    proptest! {
        #[test]
        fn floor_matches_wide_reference(
            amount in arb_u256(),
            numerator in any::<u32>(),
            denominator in 1u32..=u32::MAX,
        ) {
            prop_assert_eq!(
                mul_div_u32_floor(amount, numerator, denominator),
                reference_floor(amount, numerator, denominator)
            );
        }

        #[test]
        fn ceil_matches_wide_reference(
            amount in arb_u256(),
            numerator in any::<u32>(),
            denominator in 1u32..=u32::MAX,
        ) {
            prop_assert_eq!(
                mul_div_u32_ceil(amount, numerator, denominator),
                reference_ceil(amount, numerator, denominator)
            );
        }
    }
}
