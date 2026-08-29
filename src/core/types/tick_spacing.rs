use crate::core::types::{
    liquidity::Liquidity, tick::TickIndex, tick_slab_indexer::TickSlabIndexer,
};
use serde::{Deserialize, Serialize};

use std::ops::Deref;

/// A validated Uniswap tick spacing represented as a positive `i16`.
///
/// Wrapping a raw `i16` in this type guarantees that any `TickSpacing` in
/// circulation is positive. The `i16` storage matches Uniswap v4's
/// tick-spacing width while preventing zero or negative spacings from reaching
/// pool math.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TickSpacing(i16);

impl TickSpacing {
    /// Smallest valid tick spacing.
    pub const MIN: Self = Self(1);
    /// Largest tick spacing representable by Uniswap v4 tick spacing.
    pub const MAX: Self = Self(i16::MAX);

    /// Attempts to construct a `TickSpacing` from a raw `i16`.
    ///
    /// Returns `None` if `spacing` is zero or negative.
    pub const fn new(spacing: i16) -> Option<Self> {
        if spacing < Self::MIN.value() {
            return None;
        }
        Some(Self(spacing))
    }

    /// Returns the underlying raw tick-spacing value as `i16`.
    #[inline(always)]
    pub const fn value(&self) -> i16 {
        self.0
    }

    /// Returns the underlying raw tick-spacing value as `u32`.
    #[inline(always)]
    pub const fn as_u32(&self) -> u32 {
        self.0 as u32
    }

    /// Returns the underlying raw tick-spacing value as `i32`.
    #[inline(always)]
    pub const fn as_i32(&self) -> i32 {
        self.0 as i32
    }

    /// Returns the maximum gross liquidity allowed at one initialized tick.
    ///
    /// Derived from the number of usable compressed ticks between
    /// [`TickIndex::MIN`] and [`TickIndex::MAX`], matching Uniswap v4's
    /// `Pool.tickSpacingToMaxLiquidityPerTick`.
    #[inline(always)]
    pub const fn max_liquidity_per_tick(&self) -> Liquidity {
        let tick_spacing = self.as_i32();
        let min_compressed = TickIndex::MIN.value().div_euclid(tick_spacing);
        let max_compressed = TickIndex::MAX.value().div_euclid(tick_spacing);
        let num_ticks = (max_compressed - min_compressed + 1) as u128;
        Liquidity::new(u128::MAX / num_ticks)
    }
}

/// Calculates the number of page buckets required by `TickSlabIndexer`.
#[inline(always)]
pub(crate) const fn calculate_cache_cap(tick_spacing: TickSpacing) -> u16 {
    TickSlabIndexer::from_tick(TickIndex::MAX, tick_spacing.as_u32()).cache_index() + 1
}

/// Constructs a `TickSpacing` from a literal/expression, panicking if the value
/// is out of range.
///
/// Intended for use with compile-time-known or trusted values (tests,
/// constants, config) where an invalid spacing indicates a programming error
/// rather than recoverable input. For untrusted/runtime input, prefer
/// `TickSpacing::new` and handle the `None` case explicitly.
#[macro_export]
macro_rules! tick_spacing {
    ($val:expr) => {{
        $crate::v4::TickSpacing::new($val)
            .unwrap_or_else(|| panic!("invalid TickSpacing value: {}", $val))
    }};
}

/// Allows a `TickSpacing` to be transparently dereferenced to `&i16`, enabling
/// ergonomic use in arithmetic/comparisons without repeatedly calling
/// `.value()`.
impl Deref for TickSpacing {
    type Target = i16;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for TickSpacing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keep Display explicit so logs distinguish validated spacing from raw integers.
        write!(f, "TickSpacing({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_TICK_SPACING: i16 = TickSpacing::MIN.value();
    const MAX_TICK_SPACING: i16 = TickSpacing::MAX.value();

    // ---------- TickSpacing::new ----------

    #[test]
    fn new_accepts_min_tick_spacing() {
        let spacing = TickSpacing::new(MIN_TICK_SPACING);
        assert!(spacing.is_some());
        assert_eq!(spacing.unwrap().value(), MIN_TICK_SPACING);
    }

    #[test]
    fn new_accepts_max_tick_spacing() {
        let spacing = TickSpacing::new(MAX_TICK_SPACING);
        assert!(spacing.is_some());
        assert_eq!(spacing.unwrap().value(), MAX_TICK_SPACING);
    }

    #[test]
    fn new_accepts_value_inside_range() {
        let raw = 60;
        let spacing = TickSpacing::new(raw);
        assert!(spacing.is_some());
        assert_eq!(spacing.unwrap().value(), raw);
    }

    #[test]
    fn new_rejects_zero() {
        assert!(TickSpacing::new(0).is_none());
    }

    #[test]
    fn new_rejects_negative_value() {
        assert!(TickSpacing::new(-1).is_none());
    }

    #[test]
    fn new_rejects_i16_min() {
        assert!(TickSpacing::new(i16::MIN).is_none());
    }

    #[test]
    fn new_is_const_evaluable() {
        // Ensures `new` remains usable in const contexts.
        const SPACING: Option<TickSpacing> = TickSpacing::new(1);
        assert!(SPACING.is_some());
    }

    // ---------- value() / as_u32() / as_i32() ----------

    #[test]
    fn value_roundtrips_input() {
        let raw = 200;
        let spacing = TickSpacing::new(raw).unwrap();
        assert_eq!(spacing.value(), raw);
    }

    #[test]
    fn as_u32_returns_unsigned_value() {
        let spacing = TickSpacing::new(200).unwrap();
        assert_eq!(spacing.as_u32(), 200u32);
    }

    #[test]
    fn as_i32_returns_widened_signed_value() {
        let spacing = TickSpacing::new(200).unwrap();
        assert_eq!(spacing.as_i32(), 200i32);
    }

    // ---------- max_liquidity_per_tick() ----------

    #[test]
    fn max_liquidity_per_tick_matches_formula() {
        for &raw in &[1i16, 10, 60, 200, i16::MAX] {
            let spacing = TickSpacing::new(raw).unwrap();
            let tick_spacing = raw as i32;
            let min_compressed = TickIndex::MIN.value().div_euclid(tick_spacing);
            let max_compressed = TickIndex::MAX.value().div_euclid(tick_spacing);
            let num_ticks = (max_compressed - min_compressed + 1) as u128;

            assert_eq!(
                spacing.max_liquidity_per_tick(),
                Liquidity::new(u128::MAX / num_ticks),
                "mismatch at tick_spacing={raw}"
            );
        }
    }

    #[test]
    fn cache_capacity_covers_spacing_compressed_protocol_range() {
        let cases = [(1, 60_496), (10, 6_869), (32_767, 3)];

        for (raw_spacing, expected_capacity) in cases {
            let spacing = TickSpacing::new(raw_spacing).unwrap();

            assert_eq!(calculate_cache_cap(spacing), expected_capacity);
        }
    }

    // ---------- Deref ----------

    #[test]
    fn deref_gives_raw_i16_reference() {
        let spacing = TickSpacing::new(100).unwrap();
        let r: &i16 = &spacing;
        assert_eq!(*r, 100);
    }

    #[test]
    fn deref_supports_arithmetic_without_value_call() {
        let spacing = TickSpacing::new(100).unwrap();
        assert_eq!(*spacing + 1, 101);
        assert_eq!(*spacing - 1, 99);
        assert!(*spacing < 200);
        assert!(*spacing > 50);
    }

    #[test]
    fn deref_equality_matches_value() {
        let spacing = TickSpacing::new(42).unwrap();
        assert_eq!(*spacing, spacing.value());
    }

    // ---------- Derived traits ----------

    #[test]
    fn copy_and_clone_produce_equal_instances() {
        let spacing1 = TickSpacing::new(7).unwrap();
        let spacing2 = spacing1; // Copy
        let spacing3 = spacing1.clone(); // Clone
        assert_eq!(spacing1, spacing2);
        assert_eq!(spacing1, spacing3);
    }

    #[test]
    fn partial_eq_distinguishes_different_values() {
        let a = TickSpacing::new(1).unwrap();
        let b = TickSpacing::new(2).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn debug_format_contains_inner_value() {
        let spacing = TickSpacing::new(99).unwrap();
        let s = format!("{:?}", spacing);
        assert!(s.contains("99"));
    }

    // ---------- tick_spacing! macro ----------

    #[test]
    fn macro_constructs_valid_tick_spacing() {
        let spacing = tick_spacing!(100);
        assert_eq!(spacing.value(), 100);
    }

    #[test]
    fn macro_constructs_boundary_tick_spacings() {
        let min = tick_spacing!(MIN_TICK_SPACING);
        let max = tick_spacing!(MAX_TICK_SPACING);
        assert_eq!(min.value(), MIN_TICK_SPACING);
        assert_eq!(max.value(), MAX_TICK_SPACING);
    }

    #[test]
    #[should_panic(expected = "invalid TickSpacing value")]
    fn macro_panics_on_zero() {
        let _ = tick_spacing!(0);
    }

    #[test]
    #[should_panic(expected = "invalid TickSpacing value")]
    fn macro_panics_on_negative_value() {
        let _ = tick_spacing!(-1);
    }

    #[test]
    fn macro_accepts_expression_argument() {
        let base = 10;
        let spacing = tick_spacing!(base * 2);
        assert_eq!(spacing.value(), 20);
    }
}
