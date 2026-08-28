//! Validated tick indexes for the Uniswap price grid.
//!
//! A tick is the discrete log-price coordinate used by Uniswap pool math. This
//! module keeps raw `i32` values behind [`TickIndex`] once they have been
//! checked against the protocol tick range, so the rest of the simulator can
//! pass ticks around without repeating boundary checks.

use serde::{Deserialize, Serialize};
use std::ops::Deref;

use crate::{
    core::types::{sqrt_price::SqrtPriceX96, tick_spacing::TickSpacing},
    v4::tick_math::get_sqrt_price_at_tick,
};

/// A validated tick index within the Uniswap v4 protocol range.
///
/// Wrapping a raw `i32` in this type guarantees that any `TickIndex` in
/// circulation is inside [`Self::MIN`]..=[`Self::MAX`]. This mirrors
/// [`SqrtPriceX96`]: callers validate once at the boundary, then pass a compact
/// domain type through pool math, tick spacing checks, and price conversion.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TickIndex(i32);

impl TickIndex {
    /// Highest tick accepted by Uniswap v4 `TickMath`.
    pub const MAX: Self = Self(887_272);

    /// Lowest tick accepted by Uniswap v4 `TickMath`.
    pub const MIN: Self = Self(-887_272);

    /// Number of bits required to represent the signed protocol tick range.
    ///
    /// The slab indexer uses this width when projecting signed ticks into an
    /// order-preserving unsigned key space.
    pub const MAX_BITS: u32 = 32 - (TickIndex::MAX.value() as u32).leading_zeros() + 1;

    /// Attempts to construct a `TickIndex` from a raw `i32`.
    ///
    /// Returns `None` if `index` falls outside the inclusive
    /// [`Self::MIN`]..=[`Self::MAX`] range defined by Uniswap tick math.
    pub const fn new(index: i32) -> Option<Self> {
        if index < Self::MIN.value() || index > Self::MAX.value() {
            return None;
        }
        Some(Self(index))
    }

    /// Constructs a `TickIndex` without checking protocol bounds.
    ///
    /// # Safety
    ///
    /// Callers must ensure `index` is inside [`Self::MIN`]..=[`Self::MAX`].
    /// Prefer [`Self::new`] at API boundaries and use this only when a preceding
    /// calculation already proves the range.
    #[inline(always)]
    pub const unsafe fn new_unchecked(index: i32) -> Self {
        Self(index)
    }

    /// Returns the underlying raw tick value as `i32`.
    pub const fn value(&self) -> i32 {
        self.0
    }

    /// Returns the square root of the price at this tick, scaled by 2^96.
    ///
    /// This is the forward Uniswap tick conversion. The result is always within
    /// the validated [`SqrtPriceX96`] range because `self` is already bounded by
    /// [`Self::MIN`] and [`Self::MAX`].
    pub fn sqrt_price_x96(&self) -> SqrtPriceX96 {
        get_sqrt_price_at_tick(*self)
    }

    /// Returns whether this tick can be used as an initialized boundary for
    /// the given pool tick spacing.
    ///
    /// Uniswap pools only allow positions to start and end on ticks that are
    /// exact multiples of their configured spacing. Callers must pass a valid
    /// non-zero pool tick spacing.
    pub fn for_spacing(&self, tick_spacing: TickSpacing) -> bool {
        self.value() % tick_spacing.as_i32() == 0
    }
}

/// Constructs a `TickIndex` from a literal/expression, panicking if the
/// value is out of range.
///
/// Intended for use with compile-time-known or trusted values (tests,
/// constants, config) where an invalid tick indicates a programming
/// error rather than recoverable input. For untrusted/runtime input,
/// prefer `TickIndex::new` and handle the `None` case explicitly.
#[macro_export]
macro_rules! tick_idx {
    ($val:expr) => {{
        $crate::v4::TickIndex::new($val)
            .unwrap_or_else(|| panic!("invalid TickIndex value: {}", $val))
    }};
}

/// Allows a `TickIndex` to be transparently dereferenced to `&i32`,
/// enabling ergonomic use in arithmetic/comparisons without repeatedly
/// calling `.value()`.
impl Deref for TickIndex {
    type Target = i32;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for TickIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keep Display explicit so logs distinguish validated ticks from raw integers.
        write!(f, "TickIndex({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_TICK: i32 = TickIndex::MIN.value();
    const MAX_TICK: i32 = TickIndex::MAX.value();

    // ---------- TickIndex::new ----------

    #[test]
    fn new_accepts_min_tick() {
        let t = TickIndex::new(MIN_TICK);
        assert!(t.is_some());
        assert_eq!(t.unwrap().value(), MIN_TICK);
    }

    #[test]
    fn new_accepts_max_tick() {
        let t = TickIndex::new(MAX_TICK);
        assert!(t.is_some());
        assert_eq!(t.unwrap().value(), MAX_TICK);
    }

    #[test]
    fn new_accepts_zero() {
        let t = TickIndex::new(0);
        assert!(t.is_some());
        assert_eq!(t.unwrap().value(), 0);
    }

    #[test]
    fn new_accepts_value_inside_range() {
        let mid = MIN_TICK / 2;
        let t = TickIndex::new(mid);
        assert!(t.is_some());
        assert_eq!(t.unwrap().value(), mid);
    }

    #[test]
    fn new_rejects_below_min_tick() {
        assert!(TickIndex::new(MIN_TICK - 1).is_none());
    }

    #[test]
    fn new_rejects_above_max_tick() {
        assert!(TickIndex::new(MAX_TICK + 1).is_none());
    }

    #[test]
    fn new_rejects_i32_min() {
        assert!(TickIndex::new(i32::MIN).is_none());
    }

    #[test]
    fn new_rejects_i32_max() {
        assert!(TickIndex::new(i32::MAX).is_none());
    }

    #[test]
    fn new_is_const_evaluable() {
        // Ensures `new` remains usable in const contexts.
        const T: Option<TickIndex> = TickIndex::new(0);
        assert!(T.is_some());
    }

    // ---------- value() ----------

    #[test]
    fn value_roundtrips_input() {
        let raw = 12345;
        let t = TickIndex::new(raw).unwrap();
        assert_eq!(t.value(), raw);
    }

    #[test]
    fn value_roundtrips_negative_input() {
        let raw = -54321;
        let t = TickIndex::new(raw).unwrap();
        assert_eq!(t.value(), raw);
    }

    // ---------- Deref ----------

    #[test]
    fn deref_gives_raw_i32_reference() {
        let t = TickIndex::new(100).unwrap();
        let r: &i32 = &t;
        assert_eq!(*r, 100);
    }

    #[test]
    fn deref_supports_arithmetic_without_value_call() {
        let t = TickIndex::new(100).unwrap();
        // Exercises ergonomic arithmetic via Deref coercion.
        assert_eq!(*t + 1, 101);
        assert_eq!(*t - 1, 99);
        assert!(*t < 200);
        assert!(*t > 50);
    }

    #[test]
    fn deref_equality_matches_value() {
        let t = TickIndex::new(42).unwrap();
        assert_eq!(*t, t.value());
    }

    // ---------- Derived traits ----------

    #[test]
    fn copy_and_clone_produce_equal_instances() {
        let t1 = TickIndex::new(7).unwrap();
        let t2 = t1; // Copy
        let t3 = t1.clone(); // Clone
        assert_eq!(t1, t2);
        assert_eq!(t1, t3);
    }

    #[test]
    fn partial_eq_distinguishes_different_values() {
        let a = TickIndex::new(1).unwrap();
        let b = TickIndex::new(2).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn debug_format_contains_inner_value() {
        let t = TickIndex::new(99).unwrap();
        let s = format!("{:?}", t);
        assert!(s.contains("99"));
    }

    // ---------- tick_idx! macro ----------

    #[test]
    fn macro_constructs_valid_tick() {
        let t = tick_idx!(100);
        assert_eq!(t.value(), 100);
    }

    #[test]
    fn macro_constructs_boundary_ticks() {
        let min = tick_idx!(MIN_TICK);
        let max = tick_idx!(MAX_TICK);
        assert_eq!(min.value(), MIN_TICK);
        assert_eq!(max.value(), MAX_TICK);
    }

    #[test]
    #[should_panic(expected = "invalid TickIndex value")]
    fn macro_panics_on_value_above_max() {
        let _ = tick_idx!(MAX_TICK + 1);
    }

    #[test]
    #[should_panic(expected = "invalid TickIndex value")]
    fn macro_panics_on_value_below_min() {
        let _ = tick_idx!(MIN_TICK - 1);
    }

    #[test]
    fn macro_accepts_expression_argument() {
        let base = 10;
        let t = tick_idx!(base * 2);
        assert_eq!(t.value(), 20);
    }
}
