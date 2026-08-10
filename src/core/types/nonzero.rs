use ruint::aliases::U256;

use crate::core::types::liquidity::Liquidity;

/// A `U256` that is guaranteed to be non-zero.
pub struct NonZeroU256(U256);

impl NonZeroU256 {
    /// Attempts to construct a `NonZeroU256` from a raw `U256`.
    pub fn new(value: U256) -> Option<Self> {
        if value.is_zero() {
            None
        } else {
            Some(Self(value))
        }
    }

    /// Constructs a `NonZeroU256` without checking for zero.
    ///
    /// # Safety
    /// Callers must ensure `value` is non-zero.
    pub unsafe fn new_unchecked(value: U256) -> Self {
        Self(value)
    }

    /// Returns the underlying value by reference.
    pub fn value(&self) -> &U256 {
        &self.0
    }

    /// Returns the underlying value.
    pub fn unwrap(self) -> U256 {
        self.0
    }
}

/// A `Liquidity` value that is guaranteed to be non-zero.
pub struct NonZeroLiquidity(Liquidity);

impl NonZeroLiquidity {
    /// Attempts to construct `NonZeroLiquidity` from raw liquidity.
    pub fn new(value: u128) -> Option<Self> {
        if value == 0 {
            None
        } else {
            Some(Self(Liquidity::new(value)))
        }
    }

    /// Constructs `NonZeroLiquidity` without checking for zero.
    ///
    /// # Safety
    /// Callers must ensure `value` is non-zero.
    pub unsafe fn new_unchecked(value: Liquidity) -> Self {
        Self(value)
    }

    /// Returns the underlying liquidity by reference.
    pub fn value(&self) -> &Liquidity {
        &self.0
    }

    /// Returns the underlying liquidity.
    pub fn unwrap(self) -> Liquidity {
        self.0
    }
}
