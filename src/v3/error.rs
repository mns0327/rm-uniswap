use crate::v3::{
    pool::{MAX_SWAP_ITERATIONS, MIN_TICK_SPACING},
    tick_math::{TickMathError, MAX_TICK_SPACING},
    SwapMathError,
};

/// All failure modes that the swap simulator can produce.
///
/// Variants are `Copy` so callers can propagate them without cloning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapSimError {
    /// The supplied price limit is already on the wrong side of the current
    /// pool price (or equal to it), so the swap cannot make progress.
    PriceLimitAlreadyExceeded,

    /// The supplied price limit lies outside the valid sqrt-price range.
    PriceLimitOutOfBounds,

    /// The pool's current `sqrt_price_x96` is outside the valid range.
    InvalidPoolSqrtPrice,

    /// A tick index (pool tick or tick entry) is outside `[MIN_TICK, MAX_TICK]`.
    InvalidTick,

    /// The tick array claims a tick is initialized, but binary search cannot
    /// locate it — the array is likely unsorted or contains duplicates.
    MissingInitializedTick,

    /// Applying a `liquidity_net` delta would underflow `u128`.
    LiquidityUnderflow,

    /// Applying a `liquidity_net` delta would overflow `u128`.
    LiquidityOverflow,

    /// An intermediate or final amount does not fit in the target integer type.
    AmountOverflow,

    /// `fee` is ≥ 1 000 000 pips (≥ 100 %), which is invalid.
    FeeTooLarge,

    /// `tick_spacing` is outside `[MIN_TICK_SPACING, MAX_TICK_SPACING]`.
    InvalidTickSpacing,

    /// The swap loop exceeded [`MAX_SWAP_ITERATIONS`].
    IterationLimitExceeded,

    /// An error propagated from the `tick_math` module.
    TickMath(TickMathError),

    /// An error propagated from the `swap_math` module.
    SwapMath(SwapMathError),
}

impl From<TickMathError> for SwapSimError {
    #[inline]
    fn from(e: TickMathError) -> Self {
        SwapSimError::TickMath(e)
    }
}

impl From<SwapMathError> for SwapSimError {
    #[inline]
    fn from(e: SwapMathError) -> Self {
        SwapSimError::SwapMath(e)
    }
}

impl std::fmt::Display for SwapSimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PriceLimitAlreadyExceeded => write!(
                f,
                "price limit is already on the wrong side of the pool price"
            ),
            Self::PriceLimitOutOfBounds => {
                write!(f, "price limit is outside the valid sqrt-price range")
            }
            Self::InvalidPoolSqrtPrice => write!(f, "pool sqrt price is out of valid range"),
            Self::InvalidTick => write!(f, "tick index out of [MIN_TICK, MAX_TICK]"),
            Self::MissingInitializedTick => {
                write!(f, "initialized tick not found in sorted tick array")
            }
            Self::LiquidityUnderflow => write!(f, "liquidity delta would underflow u128"),
            Self::LiquidityOverflow => write!(f, "liquidity delta would overflow u128"),
            Self::AmountOverflow => write!(f, "amount does not fit in target integer type"),
            Self::FeeTooLarge => write!(f, "fee must be < 1_000_000 pips (< 100%)"),
            Self::InvalidTickSpacing => write!(
                f,
                "tick_spacing must be in [{MIN_TICK_SPACING}, {MAX_TICK_SPACING}]"
            ),
            Self::IterationLimitExceeded => write!(
                f,
                "swap loop exceeded {MAX_SWAP_ITERATIONS} iterations; tick array may be malformed"
            ),
            Self::TickMath(e) => write!(f, "tick math error: {e:?}"),
            Self::SwapMath(e) => write!(f, "swap math error: {e:?}"),
        }
    }
}

impl std::error::Error for SwapSimError {}
