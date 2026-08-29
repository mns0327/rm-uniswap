use ruint::aliases::U256;
use serde::{Deserialize, Serialize};
use std::{fmt::Display, ops::Deref};

/// A Uniswap liquidity amount stored at the protocol width of `uint128`.
///
/// Wrapping raw liquidity in this type keeps the protocol width explicit at
/// the type level. Since the inner value is already `u128`, every value is
/// representable and construction cannot fail.
///
/// This type intentionally does not enforce non-zero liquidity. Code paths
/// that divide by liquidity or rely on active liquidity should validate that
/// separately, typically with `NonZeroLiquidity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Liquidity(u128);

impl Liquidity {
    /// The additive identity for liquidity.
    pub const ZERO: Self = Self(0);

    /// The maximum liquidity representable by the Uniswap `uint128` domain.
    pub const MAX: Self = Self(u128::MAX);

    /// Constructs a `Liquidity` from a raw `u128`.
    ///
    /// This constructor is infallible because `u128` exactly matches the
    /// protocol's liquidity width.
    #[inline(always)]
    #[must_use]
    pub const fn new(liquidity: u128) -> Self {
        Self(liquidity)
    }

    /// Returns the underlying raw liquidity value as `u128`.
    #[inline(always)]
    #[must_use]
    pub const fn value(&self) -> u128 {
        self.0
    }

    /// Returns this liquidity amount widened to `U256`.
    ///
    /// The numeric value is unchanged; only the integer width changes so it can
    /// be used by fixed-point math without overflowing intermediate products.
    #[inline(always)]
    #[must_use]
    pub fn as_u256(&self) -> U256 {
        U256::from(self.0)
    }

    /// Returns `true` when the liquidity amount is zero.
    #[inline(always)]
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// Returns the number of bits used to represent this liquidity amount.
    ///
    /// `Liquidity::ZERO.bit_len()` returns `0`; otherwise the result is in
    /// `1..=128`.
    #[inline(always)]
    #[must_use]
    pub const fn bit_len(&self) -> u32 {
        128 - self.leading_zeros()
    }

    /// Returns the number of leading zero bits in the raw `uint128` value.
    #[must_use]
    pub const fn leading_zeros(&self) -> u32 {
        self.0.leading_zeros()
    }

    /// Returns this liquidity shifted into Q128.96 numerator form.
    ///
    /// Several Uniswap price formulas use `liquidity << 96` as a numerator.
    /// The result always fits in `U256` because the largest `uint128` value
    /// shifted left by 96 occupies 224 bits.
    #[inline(always)]
    #[must_use]
    pub fn q96(&self) -> U256 {
        U256::from(self.0) << 96
    }
}

/// Constructs a `Liquidity` from a `u128` literal or expression.
///
/// # Examples
///
/// ```
/// # use rm_uniswap::liquidity;
/// let liquidity = liquidity!(1_000_000u128);
/// assert_eq!(liquidity.value(), 1_000_000);
/// ```
#[macro_export]
macro_rules! liquidity {
    ($val:expr) => {{ $crate::v4::Liquidity::new($val) }};
}

impl From<u128> for Liquidity {
    #[inline(always)]
    fn from(value: u128) -> Self {
        Self::new(value)
    }
}

/// Allows `Liquidity` to be transparently dereferenced to `&u128`,
/// enabling ergonomic use without repeatedly calling `.value()`.
impl Deref for Liquidity {
    type Target = u128;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Display for Liquidity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Liquidity({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn new_accepts_zero() {
        let liquidity = Liquidity::new(0);
        assert_eq!(liquidity.value(), 0);
        assert!(liquidity.is_zero());
    }

    #[test]
    fn new_accepts_max() {
        let liquidity = Liquidity::new(u128::MAX);
        assert_eq!(liquidity.value(), u128::MAX);
        assert_eq!(liquidity, Liquidity::MAX);
    }

    #[test]
    fn value_roundtrips_input() {
        let raw = 123_456_789u128;
        let liquidity = Liquidity::new(raw);
        assert_eq!(liquidity.value(), raw);
    }

    #[test]
    fn constants_match_protocol_bounds() {
        assert_eq!(Liquidity::ZERO.value(), 0);
        assert_eq!(Liquidity::MAX.value(), u128::MAX);
        assert!(Liquidity::ZERO.is_zero());
        assert!(!Liquidity::MAX.is_zero());
    }

    #[test]
    fn as_u256_widens_value() {
        let raw = 123_456_789u128;
        let liquidity = Liquidity::new(raw);
        assert_eq!(liquidity.as_u256(), U256::from(raw));
    }

    #[test]
    fn as_u256_preserves_max_value() {
        assert_eq!(Liquidity::MAX.as_u256(), U256::from(u128::MAX));
    }

    #[test]
    fn bit_len_matches_u128_boundaries() {
        assert_eq!(Liquidity::ZERO.bit_len(), 0);
        assert_eq!(Liquidity::new(1).bit_len(), 1);
        assert_eq!(Liquidity::new(1u128 << 127).bit_len(), 128);
        assert_eq!(Liquidity::MAX.bit_len(), 128);
    }

    #[test]
    fn leading_zeros_matches_raw_u128() {
        for raw in [0, 1, 2, 3, 1u128 << 64, 1u128 << 127, u128::MAX] {
            assert_eq!(Liquidity::new(raw).leading_zeros(), raw.leading_zeros());
        }
    }

    #[test]
    fn q96_shifts_liquidity_by_fixed_point_fractional_width() {
        let liquidity = Liquidity::new(5);
        assert_eq!(liquidity.q96(), U256::from(5) << 96);
    }

    #[test]
    fn q96_max_value_fits_inside_u256() {
        let shifted = Liquidity::MAX.q96();
        assert_eq!(shifted, U256::from(u128::MAX) << 96);
        assert_eq!(shifted.bit_len(), 224);
    }

    #[test]
    fn deref_gives_raw_u128_reference() {
        let liquidity = Liquidity::new(100);
        let raw: &u128 = &liquidity;
        assert_eq!(*raw, 100);
    }

    #[test]
    fn copy_and_clone_produce_equal_instances() {
        let a = Liquidity::new(7);
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn ordering_uses_raw_liquidity_value() {
        let mut values = BTreeSet::new();
        values.insert(Liquidity::new(10));
        values.insert(Liquidity::ZERO);
        values.insert(Liquidity::new(5));
        values.insert(Liquidity::MAX);

        let ordered: Vec<u128> = values
            .into_iter()
            .map(|liquidity| liquidity.value())
            .collect();
        assert_eq!(ordered, vec![0, 5, 10, u128::MAX]);
    }

    #[test]
    fn partial_eq_distinguishes_different_values() {
        let a = Liquidity::new(1);
        let b = Liquidity::new(2);
        assert_ne!(a, b);
    }

    #[test]
    fn debug_format_contains_inner_value() {
        let liquidity = Liquidity::new(99);
        let s = format!("{:?}", liquidity);
        assert!(s.contains("99"));
    }

    #[test]
    fn display_format_contains_type_name_and_value() {
        let liquidity = Liquidity::new(42);
        assert_eq!(liquidity.to_string(), "Liquidity(42)");
    }

    #[test]
    fn macro_constructs_liquidity() {
        let liquidity = liquidity!(100u128);
        assert_eq!(liquidity.value(), 100);
    }

    #[test]
    fn macro_accepts_expression() {
        let base = 40u128;
        let liquidity = liquidity!(base + 2);
        assert_eq!(liquidity.value(), 42);
    }

    #[test]
    fn serde_json_is_transparent_number() {
        let liquidity = Liquidity::new(123_456);
        let json = serde_json::to_string(&liquidity).unwrap();
        assert_eq!(json, "123456");

        let decoded: Liquidity = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, liquidity);
    }

    #[test]
    fn serde_json_roundtrips_protocol_bounds() {
        for liquidity in [Liquidity::ZERO, Liquidity::MAX] {
            let json = serde_json::to_string(&liquidity).unwrap();
            let decoded: Liquidity = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, liquidity);
        }
    }
}
