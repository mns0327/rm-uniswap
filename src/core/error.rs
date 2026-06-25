use core::fmt;

/// Compact error code shared by every public operation in this crate.
///
/// The enum is intentionally payload-free and one byte wide. Errors are
/// propagated as stable reason codes; formatting is deferred until an error
/// is actually displayed.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Error {
    ZeroDenominator,
    Overflow,
    SignedOverflow,
    I128Overflow,
    ZeroPrice,
    ZeroLiquidity,
    PriceOverflow,
    PriceUnderflow,
    InsufficientToken0Reserves,
    FeeTooLarge,
    MaxFeeExactOut,
    InvalidTick,
    InvalidSqrtPrice,
    InvalidTickSpacing,
    ZeroValue,
    PriceLimitAlreadyExceeded,
    PriceLimitOutOfBounds,
    InvalidPoolSqrtPrice,
    MissingInitializedTick,
    LiquidityUnderflow,
    LiquidityOverflow,
    AmountOverflow,
    IterationLimitExceeded,
    PositionNotFound,
    PositionAlreadyExists,
    PositionNotEmpty,
    Unauthorized,
    SlippageExceeded,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroDenominator => "division by zero",
            Self::Overflow => "arithmetic result exceeds the supported integer range",
            Self::SignedOverflow => "signed arithmetic overflow",
            Self::I128Overflow => "value does not fit in i128",
            Self::ZeroPrice => "sqrt price must be non-zero",
            Self::ZeroLiquidity => "liquidity must be non-zero",
            Self::PriceOverflow => "sqrt price arithmetic overflow",
            Self::PriceUnderflow => "sqrt price arithmetic underflow",
            Self::InsufficientToken0Reserves => "insufficient token0 reserves",
            Self::FeeTooLarge => "fee must be below 1_000_000 pips",
            Self::MaxFeeExactOut => "exact-output swap with a 100% fee is undefined",
            Self::InvalidTick => "tick is outside the valid range or spacing",
            Self::InvalidSqrtPrice => "sqrt price is outside the valid range",
            Self::InvalidTickSpacing => "tick spacing is outside the valid range",
            Self::ZeroValue => "a non-zero value is required",
            Self::PriceLimitAlreadyExceeded => {
                "price limit is on the wrong side of the current pool price"
            }
            Self::PriceLimitOutOfBounds => "price limit is outside the valid sqrt-price range",
            Self::InvalidPoolSqrtPrice => "pool sqrt price is outside the valid range",
            Self::MissingInitializedTick => "initialized tick is missing from the tick store",
            Self::LiquidityUnderflow => "liquidity delta would underflow",
            Self::LiquidityOverflow => "liquidity delta would overflow",
            Self::AmountOverflow => "amount does not fit in the target integer type",
            Self::IterationLimitExceeded => "swap iteration limit exceeded",
            Self::PositionNotFound => "position does not exist",
            Self::PositionAlreadyExists => "position already exists",
            Self::PositionNotEmpty => "position still has liquidity or uncollected tokens",
            Self::Unauthorized => "caller is not authorized for this position",
            Self::SlippageExceeded => "position amount exceeds the configured slippage limit",
        })
    }
}

impl core::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn error_code_is_one_byte() {
        assert_eq!(core::mem::size_of::<Error>(), 1);
        assert_eq!(core::mem::size_of::<Result<(), Error>>(), 1);
    }
}
