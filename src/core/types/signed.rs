use std::fmt;

use ruint::aliases::U256;

use crate::Error;

// ---------------------------------------------------------------------------
// Core type
// ---------------------------------------------------------------------------

/// A 256-bit signed integer represented as a sign-magnitude pair.
///
/// Unlike two's-complement integers this type supports the full [`U256`] range
/// for both positive and negative values.  The canonical zero is always stored
/// with `negative = false`; every constructor and mutating operation upholds
/// this invariant.
///
/// # Invariant
/// `self.negative == true` implies `self.value != U256::ZERO`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct I256 {
    negative: bool,
    value: U256,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl I256 {
    /// The additive identity `0`.
    #[inline(always)]
    pub const fn zero() -> Self {
        Self {
            negative: false,
            value: U256::ZERO,
        }
    }

    /// Largest representable negative value (magnitude = max U256).
    pub const MIN: Self = Self {
        negative: true,
        value: U256::MAX,
    };

    /// Wraps a non-negative magnitude.
    #[inline(always)]
    pub const fn positive(value: U256) -> Self {
        Self {
            negative: false,
            value,
        }
    }

    /// Wraps a non-positive magnitude.  If `value` is zero the result is
    /// canonical zero (sign is cleared).
    #[inline(always)]
    pub fn negative(value: U256) -> Self {
        Self {
            negative: !value.is_zero(),
            value,
        }
    }

    /// Construct from a raw `(negative, magnitude)` pair, normalising the
    /// sign for zero.
    #[inline(always)]
    pub fn from_raw(negative: bool, value: U256) -> Self {
        Self {
            negative: negative && !value.is_zero(),
            value,
        }
    }
}

// ---------------------------------------------------------------------------
// From / TryFrom conversions
// ---------------------------------------------------------------------------

impl From<i128> for I256 {
    fn from(v: i128) -> Self {
        if v < 0 {
            // i128::MIN.unsigned_abs() still fits in u128, so this is safe.
            Self::negative(U256::from(v.unsigned_abs()))
        } else {
            Self::positive(U256::from(v as u128))
        }
    }
}

impl From<u128> for I256 {
    #[inline]
    fn from(v: u128) -> Self {
        Self::positive(U256::from(v))
    }
}

impl From<U256> for I256 {
    #[inline]
    fn from(v: U256) -> Self {
        Self::positive(v)
    }
}

impl TryFrom<I256> for i128 {
    type Error = Error;
    fn try_from(s: I256) -> Result<Self, Self::Error> {
        let max = U256::from(i128::MAX as u128);
        if s.value > max {
            return Err(Error::I128Overflow);
        }
        let v = s.value.to::<u128>() as i128;
        if s.negative {
            v.checked_neg().ok_or(Error::SignedOverflow)
        } else {
            Ok(v)
        }
    }
}

// ---------------------------------------------------------------------------
// Predicates & accessors
// ---------------------------------------------------------------------------

impl I256 {
    /// Returns `true` if the value is zero.
    #[inline]
    pub fn is_zero(self) -> bool {
        self.value.is_zero()
    }

    /// Returns `true` if the value is strictly negative.
    #[inline]
    pub fn is_negative(self) -> bool {
        self.negative
    }

    /// Returns `true` if the value is strictly positive.
    #[inline]
    pub fn is_positive(self) -> bool {
        !self.negative && !self.value.is_zero()
    }

    /// The unsigned magnitude (absolute value).
    #[inline]
    pub fn abs(self) -> U256 {
        self.value
    }

    /// The sign as `+1`, `0`, or `-1`.
    #[inline]
    pub fn signum(self) -> i8 {
        if self.is_zero() {
            0
        } else if self.negative {
            -1
        } else {
            1
        }
    }

    /// Decomposes into `(is_negative, magnitude)`.
    #[inline]
    pub fn into_parts(self) -> (bool, U256) {
        (self.negative, self.value)
    }
}

// ---------------------------------------------------------------------------
// Checked arithmetic — returning `Option`
// ---------------------------------------------------------------------------

impl I256 {
    /// Negation.  Always succeeds (sign-magnitude has no asymmetric MIN).
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn neg(self) -> Self {
        Self::from_raw(!self.negative, self.value)
    }

    /// Checked addition.
    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        match (self.negative, rhs.negative) {
            (false, false) => {
                // (+a) + (+b)
                let v = self.value.checked_add(rhs.value)?;
                Some(Self::positive(v))
            }
            (true, true) => {
                // (-a) + (-b)
                let v = self.value.checked_add(rhs.value)?;
                Some(Self::negative(v))
            }
            (false, true) => {
                // (+a) + (-b) = a - b
                Some(Self::from_raw(
                    rhs.value > self.value,
                    if rhs.value > self.value {
                        rhs.value - self.value
                    } else {
                        self.value - rhs.value
                    },
                ))
            }
            (true, false) => {
                // (-a) + (+b) = b - a
                Some(Self::from_raw(
                    self.value > rhs.value,
                    if self.value > rhs.value {
                        self.value - rhs.value
                    } else {
                        rhs.value - self.value
                    },
                ))
            }
        }
    }

    /// Checked subtraction.
    #[inline]
    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        self.checked_add(rhs.neg())
    }

    /// Checked multiplication by an unsigned scalar.
    pub fn checked_mul_unsigned(self, rhs: U256) -> Option<Self> {
        let v = self.value.checked_mul(rhs)?;
        Some(Self::from_raw(self.negative, v))
    }

    /// Checked division by an unsigned scalar.
    pub fn checked_div_unsigned(self, rhs: U256) -> Option<Self> {
        if rhs.is_zero() {
            return None;
        }
        Some(Self::from_raw(self.negative, self.value / rhs))
    }

    /// Checked remainder by an unsigned scalar.
    pub fn checked_rem_unsigned(self, rhs: U256) -> Option<Self> {
        if rhs.is_zero() {
            return None;
        }
        Some(Self::from_raw(self.negative, self.value % rhs))
    }

    /// Saturating subtraction — clamps to zero rather than wrapping.
    pub fn saturating_sub(self, rhs: Self) -> Self {
        self.checked_sub(rhs).unwrap_or(Self::zero())
    }

    /// Saturating addition — clamps to zero rather than wrapping on overflow.
    pub fn saturating_add(self, rhs: Self) -> Self {
        self.checked_add(rhs).unwrap_or(Self::zero())
    }

    /// Saturating multiplication by an unsigned scalar.
    pub fn saturating_mul(self, rhs: U256) -> Self {
        self.checked_mul_unsigned(rhs).unwrap_or(Self::zero())
    }

    /// Saturating division by an unsigned scalar (clamps to zero on div-by-zero).
    pub fn saturating_div(self, rhs: U256) -> Self {
        self.checked_div_unsigned(rhs).unwrap_or(Self::zero())
    }
}

// ---------------------------------------------------------------------------
// Mutating helpers (used in swap-step loops)
// ---------------------------------------------------------------------------

impl I256 {
    /// Add an unsigned `rhs` to this signed value, returning an error on
    /// overflow.
    ///
    /// Used for **exact-input** swaps: `amount_remaining` starts negative and
    /// moves toward zero as input tokens (net + fee) are consumed each step.
    #[inline]
    pub fn add_unsigned(&mut self, rhs: U256) -> Result<(), Error> {
        if self.negative {
            if rhs >= self.value {
                self.value = rhs - self.value;
                self.negative = false;
            } else {
                self.value -= rhs;
            }
        } else {
            self.value = self.value.checked_add(rhs).ok_or(Error::SignedOverflow)?;
        }
        Ok(())
    }

    /// Subtract an unsigned `rhs` from this signed value, returning an error
    /// on overflow.
    ///
    /// Used for **exact-output** swaps: `amount_remaining` starts positive and
    /// moves toward zero as output tokens are filled each step.
    #[inline]
    pub fn sub_unsigned(&mut self, rhs: U256) -> Result<(), Error> {
        if self.negative {
            self.value = self.value.checked_add(rhs).ok_or(Error::SignedOverflow)?;
        } else if rhs >= self.value {
            self.value = rhs - self.value;
            self.negative = !self.value.is_zero();
        } else {
            self.value -= rhs;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

impl PartialOrd for I256 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for I256 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match (self.negative, other.negative) {
            (true, false) => Less,
            (false, true) => Greater,
            (false, false) => self.value.cmp(&other.value),
            (true, true) => other.value.cmp(&self.value), // both negative: larger magnitude ⇒ smaller
        }
    }
}

// ---------------------------------------------------------------------------
// Operator overloads
// ---------------------------------------------------------------------------

impl std::ops::Neg for I256 {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self::neg(self)
    }
}

impl std::ops::Add for I256 {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        self.checked_add(rhs)
            .expect("SignedAmount addition overflow")
    }
}

impl std::ops::Sub for I256 {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        self.checked_sub(rhs)
            .expect("SignedAmount subtraction overflow")
    }
}

impl std::ops::AddAssign for I256 {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl std::ops::SubAssign for I256 {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

impl fmt::Debug for I256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SignedAmount({}{})",
            if self.negative { "-" } else { "+" },
            self.value
        )
    }
}

impl fmt::Display for I256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.negative {
            write!(f, "-{}", self.value)
        } else {
            write!(f, "{}", self.value)
        }
    }
}

impl From<alloy::primitives::Signed<256, 4>> for I256 {
    #[inline]
    fn from(v: alloy::primitives::Signed<256, 4>) -> Self {
        Self::from_raw(v.is_negative(), v.unsigned_abs())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn u(v: u128) -> U256 {
        U256::from(v)
    }
    fn pos(v: u128) -> I256 {
        I256::positive(u(v))
    }
    fn neg(v: u128) -> I256 {
        I256::negative(u(v))
    }

    // --- invariant -----------------------------------------------------------

    #[test]
    fn negative_zero_is_canonical_zero() {
        let z = I256::negative(U256::ZERO);
        assert!(!z.negative);
        assert!(z.is_zero());
    }

    #[test]
    fn from_raw_normalises_zero() {
        let z = I256::from_raw(true, U256::ZERO);
        assert_eq!(z, I256::zero());
    }

    // --- predicates ----------------------------------------------------------

    #[test]
    fn signum_values() {
        assert_eq!(pos(5).signum(), 1);
        assert_eq!(neg(5).signum(), -1);
        assert_eq!(I256::zero().signum(), 0);
    }

    #[test]
    fn is_positive_not_for_zero() {
        assert!(!I256::zero().is_positive());
        assert!(pos(1).is_positive());
        assert!(!neg(1).is_positive());
    }

    // --- checked_add ---------------------------------------------------------

    #[test]
    fn add_same_sign() {
        assert_eq!(pos(3).checked_add(pos(4)), Some(pos(7)));
        assert_eq!(neg(3).checked_add(neg(4)), Some(neg(7)));
    }

    #[test]
    fn add_opposite_sign_no_cross() {
        assert_eq!(pos(10).checked_add(neg(3)), Some(pos(7)));
        assert_eq!(neg(10).checked_add(pos(3)), Some(neg(7)));
    }

    #[test]
    fn add_opposite_sign_exact_zero() {
        assert_eq!(pos(5).checked_add(neg(5)), Some(I256::zero()));
        assert_eq!(neg(5).checked_add(pos(5)), Some(I256::zero()));
    }

    #[test]
    fn add_opposite_sign_crossing() {
        assert_eq!(pos(3).checked_add(neg(7)), Some(neg(4)));
        assert_eq!(neg(3).checked_add(pos(7)), Some(pos(4)));
    }

    #[test]
    fn add_overflow_returns_none() {
        let big = I256::positive(U256::MAX);
        assert_eq!(big.checked_add(pos(1)), None);
    }

    // --- checked_sub ---------------------------------------------------------

    #[test]
    fn sub_basic() {
        assert_eq!(pos(10).checked_sub(pos(3)), Some(pos(7)));
        assert_eq!(pos(3).checked_sub(pos(10)), Some(neg(7)));
    }

    // --- neg -----------------------------------------------------------------

    #[test]
    fn neg_toggles_sign() {
        assert_eq!(-pos(5), neg(5));
        assert_eq!(-neg(5), pos(5));
        assert_eq!(-I256::zero(), I256::zero());
    }

    // --- ordering ------------------------------------------------------------

    #[test]
    fn ordering_across_signs() {
        assert!(neg(1) < I256::zero());
        assert!(I256::zero() < pos(1));
        assert!(neg(100) < neg(1));
        assert!(pos(1) < pos(100));
    }

    // --- mutating add_unsigned / sub_unsigned --------------------------------

    #[test]
    fn add_unsigned_exact_input_simulation() {
        // Exact-input: start at -100, consume 60 then 40.
        let mut rem = neg(100);
        rem.add_unsigned(u(60)).unwrap();
        assert_eq!(rem, neg(40));
        rem.add_unsigned(u(40)).unwrap();
        assert!(rem.is_zero());
    }

    #[test]
    fn add_unsigned_crosses_zero() {
        let mut r = neg(10);
        r.add_unsigned(u(15)).unwrap();
        assert_eq!(r, pos(5));
    }

    // #[test]
    // fn add_unsigned_overflow_detected() {
    //     let mut r = pos(U256::MAX);
    //     assert!(r.add_unsigned(u(1)).is_err());
    // }

    #[test]
    fn sub_unsigned_exact_output_simulation() {
        // Exact-output: start at +100, fill 60 then 40.
        let mut rem = pos(100);
        rem.sub_unsigned(u(60)).unwrap();
        assert_eq!(rem, pos(40));
        rem.sub_unsigned(u(40)).unwrap();
        assert!(rem.is_zero());
    }

    #[test]
    fn sub_unsigned_crosses_zero() {
        let mut r = pos(10);
        r.sub_unsigned(u(15)).unwrap();
        assert_eq!(r, neg(5));
    }

    // --- checked_mul/div/rem -------------------------------------------------

    #[test]
    fn mul_unsigned() {
        assert_eq!(pos(7).checked_mul_unsigned(u(3)), Some(pos(21)));
        assert_eq!(neg(7).checked_mul_unsigned(u(3)), Some(neg(21)));
        assert_eq!(pos(7).checked_mul_unsigned(U256::ZERO), Some(I256::zero()));
    }

    #[test]
    fn div_unsigned() {
        assert_eq!(pos(10).checked_div_unsigned(u(3)), Some(pos(3)));
        assert_eq!(neg(10).checked_div_unsigned(u(3)), Some(neg(3)));
        assert_eq!(pos(10).checked_div_unsigned(U256::ZERO), None);
    }

    #[test]
    fn rem_unsigned() {
        assert_eq!(pos(10).checked_rem_unsigned(u(3)), Some(pos(1)));
        assert_eq!(neg(10).checked_rem_unsigned(u(3)), Some(neg(1)));
        assert_eq!(pos(10).checked_rem_unsigned(U256::ZERO), None);
    }

    // --- i128 conversion -----------------------------------------------------

    #[test]
    fn to_i128_roundtrip() {
        for v in [0i128, 1, -1, i128::MAX, i128::MIN + 1] {
            let s = I256::from(v);
            let result: Result<i128, _> = s.try_into();
            assert_eq!(result.unwrap(), v);
        }
    }

    #[test]
    fn to_i128_overflow() {
        let too_big = I256::positive(U256::from(i128::MAX as u128) + U256::from(1u128));
        let result: Result<i128, _> = too_big.try_into();
        assert!(result.is_err());
    }

    // --- From<i128> ----------------------------------------------------------

    #[test]
    fn from_i128_min() {
        let s = I256::from(i128::MIN);
        assert!(s.is_negative());
        assert_eq!(s.abs(), U256::from(i128::MIN.unsigned_abs()));
    }

    // --- Display / Debug -----------------------------------------------------

    #[test]
    fn display_positive() {
        assert_eq!(pos(42).to_string(), "42");
    }

    #[test]
    fn display_negative() {
        assert_eq!(neg(42).to_string(), "-42");
    }

    #[test]
    fn display_zero() {
        assert_eq!(I256::zero().to_string(), "0");
    }

    #[test]
    fn debug_format() {
        assert_eq!(format!("{:?}", pos(5)), "SignedAmount(+5)");
        assert_eq!(format!("{:?}", neg(5)), "SignedAmount(-5)");
    }

    // --- From<alloy::primitives::Signed<256, 4>> -----------------------------

    #[test]
    fn from_alloy_signed_positive() {
        let alloy = alloy::primitives::I256::unchecked_from(42i128);
        let s = I256::from(alloy);

        assert_eq!(s, pos(42));
        assert!(s.is_positive());
    }

    #[test]
    fn from_alloy_signed_negative() {
        let alloy = alloy::primitives::I256::unchecked_from(-42i128);
        let s = I256::from(alloy);

        assert_eq!(s, neg(42));
        assert!(s.is_negative());
        assert_eq!(s.abs(), u(42));
    }

    #[test]
    fn from_alloy_signed_zero_is_canonical_zero() {
        let alloy = alloy::primitives::I256::ZERO;
        let s = I256::from(alloy);

        assert_eq!(s, I256::zero());
        assert!(!s.is_negative());
        assert!(s.is_zero());
    }

    #[test]
    fn from_alloy_signed_min_preserves_full_magnitude() {
        let alloy = alloy::primitives::I256::MIN;
        let s = I256::from(alloy);

        assert!(s.is_negative());
        assert_eq!(s.abs(), U256::from(1u8) << 255);
    }
}
