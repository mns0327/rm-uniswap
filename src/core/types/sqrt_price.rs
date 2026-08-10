use ruint::{
    ParseError,
    aliases::{U160, U256},
    uint,
};
use serde::{Deserialize, Serialize};
use std::{fmt::Display, ops::Deref};

use crate::core::{math::tick::get_tick_at_sqrt_price, types::tick::TickIndex};

/// A validated Uniswap Q64.96 square-root price stored as a `uint160`.
///
/// Wrapping a raw `U160` in this type guarantees that any `SqrtPriceX96` in
/// circulation is inside the protocol-supported sqrt-price range. This mirrors
/// [`TickIndex`]: callers validate once at the boundary, then pass a compact
/// type through pool math without repeatedly checking raw values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SqrtPriceX96(U160);

impl SqrtPriceX96 {
    /// Maximum sqrt price emitted by Uniswap `TickMath` for [`TickIndex::MAX`].
    pub const MAX: Self = Self(uint!(
        1461446703485210103287273052203988822378723970342_U160
    ));

    /// Minimum sqrt price emitted by Uniswap `TickMath` for [`TickIndex::MIN`].
    pub const MIN: Self = Self(uint!(4295128739_U160));

    /// Attempts to construct a `SqrtPriceX96` from a raw `U160`.
    ///
    /// Returns `None` if `sqrt_price_x96` falls outside the inclusive
    /// [`Self::MIN`]..=[`Self::MAX`] protocol price range.
    pub fn new(sqrt_price_x96: U160) -> Option<Self> {
        let sqrt_price = unsafe { Self::new_unchecked(sqrt_price_x96) };
        if sqrt_price.is_valid() {
            Some(sqrt_price)
        } else {
            None
        }
    }

    /// Attempts to construct a `SqrtPriceX96` from a widened integer.
    ///
    /// Values that do not fit in `uint160`, or that fit but fall outside the
    /// protocol sqrt-price range, are rejected.
    pub fn from_u256(sqrt_price_x96: U256) -> Option<Self> {
        if sqrt_price_x96.bit_len() > 160 {
            return None;
        }
        Self::new(sqrt_price_x96.to::<U160>())
    }

    /// Parses a decimal `uint160` string and validates it as a protocol sqrt price.
    ///
    /// The outer `Result` reports malformed or overflowing decimal text; the
    /// inner `Option` reports a well-formed value outside [`Self::MIN`]..=[`Self::MAX`].
    pub fn from_decimal_str(sqrt_price_x96: &str) -> Result<Option<Self>, ParseError> {
        U160::from_str_radix(sqrt_price_x96, 10).map(Self::new)
    }

    /// Constructs a `SqrtPriceX96` without checking the protocol bounds.
    ///
    /// # Safety
    ///
    /// Callers must ensure `sqrt_price_x96` is within [`Self::MIN`]..=[`Self::MAX`].
    /// Prefer [`Self::new`] at API boundaries.
    #[inline(always)]
    pub const unsafe fn new_unchecked(sqrt_price_x96: U160) -> Self {
        Self(sqrt_price_x96)
    }

    /// Returns whether the underlying value is within the protocol bounds.
    #[inline(always)]
    pub fn is_valid(&self) -> bool {
        self.0 >= Self::MIN.0 && self.0 <= Self::MAX.0
    }

    /// Returns the underlying raw Q64.96 sqrt price as `U160`.
    pub const fn value(&self) -> U160 {
        self.0
    }

    /// Returns this Q64.96 sqrt price widened to `U256`.
    pub fn as_u256(&self) -> U256 {
        self.0.to::<U256>()
    }

    /// Returns `true` if this Q64.96 sqrt price is equal to the maximum value.
    pub fn is_max(&self) -> bool {
        self.0 == Self::MAX.0
    }

    /// Returns the greatest tick whose sqrt price is less than or equal to this value.
    ///
    /// This follows the same floor semantics as Uniswap `getTickAtSqrtRatio`.
    pub fn tick_index(&self) -> TickIndex {
        get_tick_at_sqrt_price(self)
    }

    /// Adds two sqrt prices and returns `None` on integer overflow or range overflow.
    #[inline(always)]
    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        self.0.checked_add(rhs.0).and_then(|v| Self::new(v))
    }

    /// Subtracts two sqrt prices and returns `None` on integer underflow or range underflow.
    #[inline(always)]
    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        self.0.checked_sub(rhs.0).and_then(|v| Self::new(v))
    }

    /// Returns the pair ordered by ascending sqrt price.
    #[inline(always)]
    pub fn sort(self, rhs: Self) -> (Self, Self) {
        if self.0 < rhs.0 {
            (self, rhs)
        } else {
            (rhs, self)
        }
    }
}

/// Constructs a `SqrtPriceX96`, panicking if the value is out of range.
///
/// Decimal literals and `str` inputs are parsed in base 10. For already typed
/// values, use the `raw` form so the conversion remains explicit.
///
/// # Examples
///
/// ```ignore
/// let a = sqrt_price_x96!(79228162514264337593543950336);
/// let b = sqrt_price_x96!(raw some_u160_value);
///
/// let c = sqrt_price_x96!(
///     str "1461446703485210103287273052203988822378723970342"
/// );
/// ```
#[macro_export]
macro_rules! sqrt_price_x96 {
    (str $val:expr) => {{
        let text: &str = $val;

        $crate::v4::SqrtPriceX96::from_decimal_str(text)
            .unwrap_or_else(|err| panic!("invalid SqrtPriceX96 string `{}`: {:?}", text, err,))
            .unwrap_or_else(|| panic!("SqrtPriceX96 value is out of range: {}", text,))
    }};

    ($val:literal) => {{
        let text = stringify!($val);

        $crate::v4::SqrtPriceX96::from_decimal_str(text)
            .unwrap_or_else(|err| panic!("invalid U160 literal `{}`: {:?}", text, err,))
            .unwrap_or_else(|| panic!("SqrtPriceX96 value is out of range: {}", text,))
    }};

    (raw $val:expr) => {{
        let value = $val;

        $crate::v4::SqrtPriceX96::new(value)
            .unwrap_or_else(|| panic!("SqrtPriceX96 value is out of range: {}", value,))
    }};
}

/// Allows a `SqrtPriceX96` to be transparently dereferenced to `&U160`,
/// enabling ergonomic comparisons without repeatedly calling `.value()`.
impl Deref for SqrtPriceX96 {
    type Target = U160;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Display for SqrtPriceX96 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SqrtPriceX96({})", self.value())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_idx;

    fn sqrt_price_1_1() -> U160 {
        U160::from(1u128 << 96)
    }

    // ---------- constructors and bounds ----------

    #[test]
    fn new_accepts_min_sqrt_price() {
        let sqrt_price = SqrtPriceX96::new(SqrtPriceX96::MIN.value());
        assert!(sqrt_price.is_some());
        assert_eq!(sqrt_price.unwrap(), SqrtPriceX96::MIN);
    }

    #[test]
    fn new_accepts_max_sqrt_price() {
        let sqrt_price = SqrtPriceX96::new(SqrtPriceX96::MAX.value());
        assert!(sqrt_price.is_some());
        assert_eq!(sqrt_price.unwrap(), SqrtPriceX96::MAX);
    }

    #[test]
    fn new_accepts_value_inside_range() {
        let sqrt_price = SqrtPriceX96::new(sqrt_price_1_1());
        assert!(sqrt_price.is_some());
        assert_eq!(sqrt_price.unwrap().value(), sqrt_price_1_1());
    }

    #[test]
    fn new_rejects_below_min_sqrt_price() {
        assert!(SqrtPriceX96::new(SqrtPriceX96::MIN.value() - U160::ONE).is_none());
    }

    #[test]
    fn new_rejects_above_max_sqrt_price() {
        assert!(SqrtPriceX96::new(SqrtPriceX96::MAX.value() + U160::ONE).is_none());
    }

    #[test]
    fn from_u256_accepts_valid_widened_value() {
        let raw = sqrt_price_1_1().to::<U256>();
        let sqrt_price = SqrtPriceX96::from_u256(raw).unwrap();
        assert_eq!(sqrt_price.value(), sqrt_price_1_1());
    }

    #[test]
    fn from_u256_rejects_values_wider_than_uint160() {
        assert!(SqrtPriceX96::from_u256(U256::ONE << 160).is_none());
    }

    #[test]
    fn from_u256_rejects_uint160_values_outside_protocol_range() {
        assert!(SqrtPriceX96::from_u256(U160::MAX.to::<U256>()).is_none());
    }

    #[test]
    fn from_decimal_str_accepts_valid_decimal_value() {
        let sqrt_price = SqrtPriceX96::from_decimal_str("79228162514264337593543950336").unwrap();
        assert_eq!(sqrt_price.unwrap().value(), sqrt_price_1_1());
    }

    #[test]
    fn from_decimal_str_rejects_malformed_decimal_value() {
        assert!(SqrtPriceX96::from_decimal_str("not-a-number").is_err());
    }

    #[test]
    fn from_decimal_str_rejects_well_formed_value_outside_protocol_range() {
        assert_eq!(SqrtPriceX96::from_decimal_str("4295128738").unwrap(), None);
    }

    #[test]
    fn unchecked_constructor_can_represent_invalid_values_for_internal_validation() {
        let sqrt_price = unsafe { SqrtPriceX96::new_unchecked(U160::ZERO) };
        assert!(!sqrt_price.is_valid());
    }

    // ---------- accessors and conversions ----------

    #[test]
    fn value_roundtrips_input() {
        let raw = sqrt_price_1_1();
        let sqrt_price = SqrtPriceX96::new(raw).unwrap();
        assert_eq!(sqrt_price.value(), raw);
    }

    #[test]
    fn as_u256_widens_value() {
        let raw = sqrt_price_1_1();
        let sqrt_price = SqrtPriceX96::new(raw).unwrap();
        assert_eq!(sqrt_price.as_u256(), raw.to::<U256>());
    }

    #[test]
    fn is_max_identifies_only_max_boundary() {
        assert!(SqrtPriceX96::MAX.is_max());
        assert!(!SqrtPriceX96::MIN.is_max());
        assert!(!SqrtPriceX96::new(sqrt_price_1_1()).unwrap().is_max());
    }

    #[test]
    fn tick_index_roundtrips_tick_zero_price_with_floor_semantics() {
        let sqrt_price = SqrtPriceX96::new(sqrt_price_1_1()).unwrap();
        assert_eq!(sqrt_price.tick_index(), tick_idx!(0));
    }

    #[test]
    fn tick_index_accepts_protocol_boundaries() {
        assert_eq!(SqrtPriceX96::MIN.tick_index(), TickIndex::MIN);
        assert_eq!(SqrtPriceX96::MAX.tick_index(), TickIndex::MAX);
    }

    // ---------- arithmetic helpers ----------

    #[test]
    fn checked_add_returns_sum_when_result_stays_in_range() {
        let lhs = SqrtPriceX96::MIN;
        let rhs = SqrtPriceX96::MIN;
        let sum = lhs.checked_add(rhs).unwrap();
        assert_eq!(sum.value(), lhs.value() + rhs.value());
    }

    #[test]
    fn checked_add_rejects_result_above_max() {
        assert!(SqrtPriceX96::MAX.checked_add(SqrtPriceX96::MIN).is_none());
    }

    #[test]
    fn checked_sub_returns_difference_when_result_stays_in_range() {
        let lhs = SqrtPriceX96::new(sqrt_price_1_1()).unwrap();
        let diff = lhs.checked_sub(SqrtPriceX96::MIN).unwrap();
        assert_eq!(diff.value(), lhs.value() - SqrtPriceX96::MIN.value());
    }

    #[test]
    fn checked_sub_rejects_result_below_min() {
        assert!(SqrtPriceX96::MIN.checked_sub(SqrtPriceX96::MIN).is_none());
    }

    #[test]
    fn sort_returns_values_in_ascending_order() {
        assert_eq!(
            SqrtPriceX96::MAX.sort(SqrtPriceX96::MIN),
            (SqrtPriceX96::MIN, SqrtPriceX96::MAX)
        );
    }

    // ---------- formatting and deref ----------

    #[test]
    fn deref_gives_raw_u160_reference() {
        let sqrt_price = SqrtPriceX96::new(sqrt_price_1_1()).unwrap();
        let raw: &U160 = &sqrt_price;
        assert_eq!(*raw, sqrt_price_1_1());
    }

    #[test]
    fn display_includes_type_name_and_value() {
        let sqrt_price = SqrtPriceX96::new(sqrt_price_1_1()).unwrap();
        assert_eq!(
            sqrt_price.to_string(),
            format!("SqrtPriceX96({})", sqrt_price_1_1())
        );
    }

    // ---------- sqrt_price_x96! macro ----------

    #[test]
    fn macro_constructs_valid_literal_sqrt_price() {
        let sqrt_price = sqrt_price_x96!(79228162514264337593543950336);
        assert_eq!(sqrt_price.value(), sqrt_price_1_1());
    }

    #[test]
    fn macro_constructs_valid_string_sqrt_price() {
        let sqrt_price = sqrt_price_x96!(str "79228162514264337593543950336");
        assert_eq!(sqrt_price.value(), sqrt_price_1_1());
    }

    #[test]
    fn macro_constructs_valid_raw_sqrt_price() {
        let sqrt_price = sqrt_price_x96!(raw sqrt_price_1_1());
        assert_eq!(sqrt_price.value(), sqrt_price_1_1());
    }

    #[test]
    #[should_panic(expected = "SqrtPriceX96 value is out of range")]
    fn macro_panics_on_literal_below_min() {
        let _ = sqrt_price_x96!(4295128738);
    }

    #[test]
    #[should_panic(expected = "invalid SqrtPriceX96 string")]
    fn macro_panics_on_invalid_string() {
        let _ = sqrt_price_x96!(str "not-a-number");
    }
}
