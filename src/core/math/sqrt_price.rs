//! Optimized port of Uniswap `SqrtPriceMath`.
//!
//! Design goals:
//! - Keep Uniswap-compatible rounding semantics.
//! - Avoid Solidity-style overflow checks that are expensive in Rust.
//! - Avoid generic FullMath paths when the expression has a cheaper fixed form.
//! - Reject values outside the real Uniswap sqrt-price domain instead of silently
//!   truncating left shifts.

use crate::core::types::liquidity::Liquidity;
use crate::core::types::nonzero::{NonZeroLiquidity, NonZeroU256};
use crate::core::types::sqrt_price::SqrtPriceX96;
use alloy::primitives::I256;
use ruint::Uint;
use ruint::aliases::{U128, U160, U256};

/// Number of fractional bits in Uniswap's Q64.96 sqrt-price representation.
const Q96_SHIFT: u32 = 96;

/// Wide intermediate for a `uint128 * uint160` product.
///
/// This covers both `liquidity * sqrt_price_x96` and
/// `liquidity * (sqrt_b - sqrt_a)`: liquidity is a Uniswap `uint128`, while
/// protocol sqrt prices and sqrt-price differences fit below `uint160`.
pub type U288 = Uint<288, 5>;

/// Wide numerator for multiplying a U288 intermediate by Q96.
type U384 = Uint<384, 6>;

/// Wide numerator for shifting a full U256 token1 amount by Q96.
type U352 = Uint<352, 6>;

/// Backward-compatible name for the crate-wide compact error code.
pub type SqrtPriceMathError = crate::Error;

#[inline(always)]
fn into_sqrt_price(value: U256) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    SqrtPriceX96::from_u256(value).ok_or(SqrtPriceMathError::InvalidSqrtPrice)
}

/// Computes `ceil(liquidity * sqrt_price * Q96 / denominator)`.
///
/// This replaces:
///
/// ```text
/// mul_div_rounding_up(liquidity << 96, sqrt_price, denominator)
/// ```
///
/// in the token0 next-price path.
///
/// Why this is better:
/// - `liquidity << 96` is a known Q96 form.
/// - `liquidity * sqrt_price * Q96` fits in U384 under real Uniswap bounds.
/// - We avoid the generic FullMath modular-inverse path.
#[inline(always)]
fn liquidity_sqrt_q96_div_rounding_up(
    liquidity: &NonZeroLiquidity,
    sqrt_price: &SqrtPriceX96,
    denominator: NonZeroU256,
) -> Result<U256, SqrtPriceMathError> {
    // These conversions are safe by the type invariants:
    //
    // liquidity  <= uint128
    // sqrtP      <= uint160
    let liquidity_128 = U128::from(liquidity.unwrap().value());

    let sqrt_p_160 = sqrt_price.as_u256().to::<U160>();

    // U128 × U160 = U288
    let product: U288 = liquidity_128.widening_mul(sqrt_p_160);

    // (L × sqrtP) << 96
    //
    // 288 + 96 = 384 bits max.
    let numerator: U384 = U384::from(product) << Q96_SHIFT;

    div_u384_by_u256_rounding_up(&numerator, denominator)
}

#[inline(always)]
fn div_u384_by_u256_rounding_up(
    numerator: &U384,
    denominator: NonZeroU256,
) -> Result<U256, SqrtPriceMathError> {
    // The caller supplies a non-zero denominator, so `div_ceil` cannot panic or
    // perform an invalid division.
    let quotient = numerator.div_ceil(U384::from(denominator.unwrap()));

    if quotient.bit_len() > 256 {
        return Err(SqrtPriceMathError::PriceOverflow);
    }

    Ok(quotient.to::<U256>())
}

/// Computes `amount * Q96 / liquidity` for token1-driven price movement.
///
/// The common case keeps the numerator in U256. Larger `amount` values use a
/// fixed-width U352 numerator because shifting a full U256 amount by Q96 needs
/// at most 352 bits.
///
/// `rounding_up` selects the Solidity-compatible division direction:
/// - `false`: floor division, used when token1 is added to the pool.
/// - `true`: ceil division, used when token1 is removed from the pool.
///
/// A quotient wider than `uint160` cannot be applied to a Uniswap sqrt price.
/// `overflow_error` keeps the public error meaningful for each direction: the
/// add path reports price overflow, while the remove path reports insufficient
/// token1 reserves.
#[inline(always)]
fn amount1_quotient(
    amount: U256,
    liquidity: U256,
    rounding_up: bool,
    overflow_error: SqrtPriceMathError,
) -> Result<U256, SqrtPriceMathError> {
    if amount.bit_len() <= 160 {
        // Fast path: `amount <= uint160`, so `amount << 96` is still within
        // U256 and no wide arithmetic is needed.
        let numerator = amount << Q96_SHIFT;

        return Ok(if rounding_up {
            numerator.div_ceil(liquidity)
        } else {
            numerator / liquidity
        });
    }

    // Wide path: support the full U256 `amount` domain without truncating the
    // Q96 shift.
    let numerator: U352 = U352::from(amount) << Q96_SHIFT;
    let liquidity = U352::from(liquidity);
    let quotient = if rounding_up {
        numerator.div_ceil(liquidity)
    } else {
        numerator / liquidity
    };

    if quotient.bit_len() > 160 {
        return Err(overflow_error);
    }

    Ok(quotient.to::<U256>())
}

#[inline(always)]
fn amount1_quotient_rounding_down(
    amount: U256,
    liquidity: U256,
) -> Result<U256, SqrtPriceMathError> {
    amount1_quotient(amount, liquidity, false, SqrtPriceMathError::PriceOverflow)
}

#[inline(always)]
fn amount1_quotient_rounding_up(amount: U256, liquidity: U256) -> Result<U256, SqrtPriceMathError> {
    amount1_quotient(
        amount,
        liquidity,
        true,
        SqrtPriceMathError::InsufficientToken1Reserves,
    )
}

/// Computes the next sqrt price when token0 is added to the pool.
///
/// Adding token0 decreases the price, so the denominator adds
/// `amount * sqrtP` to `liquidity * Q96`. The direct path is used whenever the
/// product and denominator fit in U256; otherwise the Solidity fallback formula
/// is used to preserve rounding and overflow behavior.
#[inline(always)]
fn get_next_sqrt_price_from_amount0_add_rounding_up(
    sqrt_price: &SqrtPriceX96,
    liquidity: &NonZeroLiquidity,
    amount: &NonZeroU256,
) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    let sqrt_p = sqrt_price.as_u256();
    let numerator1 = liquidity.q96();

    let product_bits_upper = amount.bit_len() + sqrt_p.bit_len();

    // ---------------------------------------------------------
    // Fastest path
    //
    // If:
    //
    //     bits(amount) + bits(sqrtP) <= 255
    //
    // then:
    //
    //     amount * sqrtP < 2^255
    //
    // and since:
    //
    //     numerator1 < 2^224
    //
    // we know:
    //
    //     numerator1 + product < 2^256
    //
    // So neither multiplication nor addition needs
    // overflow detection.
    // ---------------------------------------------------------
    if product_bits_upper <= 255 {
        let product = amount.as_ref() * sqrt_p;

        let denominator = numerator1 + product;

        // Safety: `liquidity` is `NonZeroLiquidity`, so `numerator1 =
        // liquidity << 96` is strictly positive. This branch only adds a
        // non-negative product and has already proven the addition cannot
        // overflow, therefore the denominator cannot be zero.
        let denominator = unsafe { NonZeroU256::new_unchecked(denominator) };

        let result = liquidity_sqrt_q96_div_rounding_up(liquidity, sqrt_price, denominator)?;

        return into_sqrt_price(result);
    }

    // ---------------------------------------------------------
    // Product is guaranteed to fit U256,
    // but denominator addition might overflow.
    // ---------------------------------------------------------
    if product_bits_upper <= 256 {
        let product = amount.as_ref() * sqrt_p;

        if let Some(denominator) = numerator1.checked_add(product) {
            // Safety: `liquidity` is `NonZeroLiquidity`, so `numerator1 =
            // liquidity << 96` is strictly positive. `checked_add` guarantees
            // the positive numerator is preserved without wrapping, and the
            // product is non-negative, so the denominator cannot be zero.
            let denominator = unsafe { NonZeroU256::new_unchecked(denominator) };

            let result = liquidity_sqrt_q96_div_rounding_up(liquidity, sqrt_price, denominator)?;

            return into_sqrt_price(result);
        }

        return amount0_add_overflow_fallback(numerator1, sqrt_p, amount);
    }

    // ---------------------------------------------------------
    // General path.
    //
    // product_bits_upper > 256 does NOT necessarily mean
    // multiplication actually overflows, so preserve exact
    // Solidity behaviour with overflowing_mul().
    // ---------------------------------------------------------
    let (product, product_overflowed) = amount.overflowing_mul(sqrt_p);

    if !product_overflowed && let Some(denominator) = numerator1.checked_add(product) {
        // Safety: `liquidity` is `NonZeroLiquidity`, so `numerator1 =
        // liquidity << 96` is strictly positive. `checked_add` guarantees
        // the positive numerator is preserved without wrapping, and the
        // product is non-negative, so the denominator cannot be zero.
        let denominator = unsafe { NonZeroU256::new_unchecked(denominator) };

        let result = liquidity_sqrt_q96_div_rounding_up(liquidity, sqrt_price, denominator)?;

        return into_sqrt_price(result);
    }

    amount0_add_overflow_fallback(numerator1, sqrt_p, amount)
}

#[cold]
#[inline(never)]
fn amount0_add_overflow_fallback(
    numerator1: U256,
    sqrt_price: U256,
    amount: &U256,
) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    // Algebraically equivalent Solidity fallback used when
    // `amount * sqrtP + numerator1` cannot be evaluated directly in U256:
    //
    // ceil(
    //     numerator1 /
    //     (floor(numerator1 / sqrtP) + amount)
    // )

    let base = numerator1 / sqrt_price;

    let denominator = base
        .checked_add(*amount)
        .ok_or(SqrtPriceMathError::PriceOverflow)?;

    // Every caller passes `amount` out of a `NonZeroU256`, so the denominator
    // is strictly positive even when `base` is zero.
    let result = numerator1.div_ceil(denominator);

    into_sqrt_price(result)
}

/// Computes the next sqrt price when token0 is removed from the pool.
///
/// Removing token0 increases the price, so the denominator subtracts
/// `amount * sqrtP` from `liquidity * Q96`. The result is rounded up to match
/// Uniswap's exact-output semantics.
#[inline(always)]
fn get_next_sqrt_price_from_amount0_sub_rounding_up(
    sqrt_price: &SqrtPriceX96,
    liquidity: &NonZeroLiquidity,
    amount: &NonZeroU256,
) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    let sqrt_p = sqrt_price.as_u256();

    let numerator1 = liquidity.q96();

    let product_bits_upper = amount.bit_len() + sqrt_p.bit_len();

    let product = if product_bits_upper <= 256 {
        // The product is guaranteed to fit in U256, so the unchecked `*`
        // cannot wrap in this branch.
        amount.as_ref() * sqrt_p
    } else {
        let (product, overflowed) = amount.overflowing_mul(sqrt_p);

        if overflowed {
            return Err(SqrtPriceMathError::InsufficientToken0Reserves);
        }

        product
    };

    if numerator1 <= product {
        return Err(SqrtPriceMathError::InsufficientToken0Reserves);
    }

    let denominator = numerator1 - product;

    // Safety: the branch above rejects `numerator1 <= product`, so this
    // subtraction is executed only when `numerator1 - product` is strictly
    // positive. That is exactly the invariant required by `NonZeroU256`.
    let denominator = unsafe { NonZeroU256::new_unchecked(denominator) };

    let result = liquidity_sqrt_q96_div_rounding_up(liquidity, sqrt_price, denominator)?;

    into_sqrt_price(result)
}

/// Computes the next sqrt price after adding or removing token0.
///
/// Formula:
///
/// add = true:
///     ceil(liq * Q96 * sqrtP / (liq * Q96 + amount * sqrtP))
///
/// add = false:
///     ceil(liq * Q96 * sqrtP / (liq * Q96 - amount * sqrtP))
///
/// Passing `add = true` models token0 entering the pool and moves price down.
/// Passing `add = false` models token0 leaving the pool and moves price up.
///
/// Solidity returns `sqrtPX96` unchanged for a zero amount. `NonZeroU256`
/// makes that case unrepresentable here, so callers must short-circuit it.
///
/// A result outside [`SqrtPriceX96::MIN`]..=[`SqrtPriceX96::MAX`] is rejected
/// with `InvalidSqrtPrice` instead of being returned as a raw `uint160`.
#[inline(always)]
pub fn get_next_sqrt_price_from_amount0_rounding_up(
    sqrt_price: &SqrtPriceX96,
    liquidity: &NonZeroLiquidity,
    amount: &NonZeroU256,
    add: bool,
) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    if add {
        get_next_sqrt_price_from_amount0_add_rounding_up(sqrt_price, liquidity, amount)
    } else {
        get_next_sqrt_price_from_amount0_sub_rounding_up(sqrt_price, liquidity, amount)
    }
}

/// Computes the next sqrt price after adding or removing token1.
///
/// Formula:
///
/// add = true:
///     sqrtP + floor(amount * Q96 / liquidity)
///
/// add = false:
///     sqrtP - ceil(amount * Q96 / liquidity)
///
/// Passing `add = true` models token1 entering the pool and moves price up.
/// Passing `add = false` models token1 leaving the pool and moves price down.
///
/// Solidity returns `sqrtPX96` unchanged for a zero amount. `NonZeroU256`
/// makes that case unrepresentable here, so callers must short-circuit it.
///
/// A result outside [`SqrtPriceX96::MIN`]..=[`SqrtPriceX96::MAX`] is rejected
/// with `InvalidSqrtPrice` instead of being returned as a raw `uint160`.
#[inline(always)]
pub fn get_next_sqrt_price_from_amount1_rounding_down(
    sqrt_price: &SqrtPriceX96,
    liquidity: &NonZeroLiquidity,
    amount: &NonZeroU256,
    add: bool,
) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    if add {
        get_next_sqrt_price_from_amount1_add_rounding_down(sqrt_price, liquidity, amount)
    } else {
        get_next_sqrt_price_from_amount1_sub_rounding_down(sqrt_price, liquidity, amount)
    }
}

/// Computes the next sqrt price when token1 is added to the pool.
///
/// Token1 movement is linear in sqrt-price space, so the price increment is
/// `floor(amount * Q96 / liquidity)`.
///
/// Zero amounts are unrepresentable; see
/// [`get_next_sqrt_price_from_amount1_rounding_down`].
#[inline(always)]
pub fn get_next_sqrt_price_from_amount1_add_rounding_down(
    sqrt_price: &SqrtPriceX96,
    liquidity: &NonZeroLiquidity,
    amount: &NonZeroU256,
) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    let sqrt_p = sqrt_price.as_u256();
    let liquidity = liquidity.unwrap().as_u256();

    let quotient = amount1_quotient_rounding_down(*amount.value(), liquidity)?;

    let result = sqrt_p
        .checked_add(quotient)
        .ok_or(SqrtPriceMathError::PriceOverflow)?;

    into_sqrt_price(result)
}

/// Computes the next sqrt price when token1 is removed from the pool.
///
/// The decrement is rounded up so exact-output swaps move the price far enough
/// to cover the requested token1 amount.
#[inline(always)]
fn get_next_sqrt_price_from_amount1_sub_rounding_down(
    sqrt_price: &SqrtPriceX96,
    liquidity: &NonZeroLiquidity,
    amount: &NonZeroU256,
) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    let sqrt_p = sqrt_price.as_u256();
    let liquidity = liquidity.unwrap().as_u256();

    let quotient = amount1_quotient_rounding_up(*amount.value(), liquidity)?;

    // Match Solidity's reserve check:
    //
    // require(sqrtPX96 > quotient);
    //
    // Equality is rejected as well.
    if sqrt_p <= quotient {
        return Err(SqrtPriceMathError::InsufficientToken1Reserves);
    }

    let result = sqrt_p - quotient;

    into_sqrt_price(result)
}

/// Computes token0 required to cover a liquidity position between two sqrt prices.
///
/// Returns `(amount0, liquidity_delta_sqrt)`.
///
/// `liquidity_delta_sqrt` is the unscaled intermediate
/// `liquidity * (sqrt_b - sqrt_a)`. Returning it lets callers reuse the same
/// product when they also need the token1 delta for this exact price range.
///
/// Formula:
///
/// ```text
/// amount0 = liquidity * Q96 * (sqrt_b - sqrt_a) / (sqrt_a * sqrt_b)
/// ```
#[inline]
pub fn get_amount0_delta(
    sqrt_price_a: &SqrtPriceX96,
    sqrt_price_b: &SqrtPriceX96,
    liquidity: Liquidity,
    round_up: bool,
) -> Result<(U256, U288), SqrtPriceMathError> {
    let (sqrt_a, sqrt_b) = sqrt_price_a.sort(sqrt_price_b);

    let sqrt_a = sqrt_a.as_u256();
    let sqrt_b = sqrt_b.as_u256();

    let diff = sqrt_b - sqrt_a;

    if diff.is_zero() {
        return Ok((U256::ZERO, U288::ZERO));
    }

    // Cache the part shared with token1 delta: liquidity * (sqrt_b - sqrt_a).
    // The product is at most 288 bits because liquidity is uint128 and the
    // sqrt-price difference is below uint160.
    let liquidity_128 = U128::from(liquidity.value());
    let diff_160 = diff.to::<U160>();
    let liquidity_delta_sqrt: U288 = liquidity_128.widening_mul(diff_160);

    let amount = get_amount0_delta_with_liquidity_delta(
        sqrt_price_a,
        sqrt_price_b,
        liquidity_delta_sqrt,
        round_up,
    )?;

    Ok((amount, liquidity_delta_sqrt))
}

/// Returns the signed amount0 delta for a given liquidity delta and price range.
///
/// The sign follows the caller's balance (Uniswap v4 `BalanceDelta`), which is
/// the inverse of v3 `SqrtPriceMath.getAmount0Delta(int128)`:
/// - `liquidity >= 0`: negative, rounded up — tokens the caller must pay.
/// - `liquidity < 0`: positive, rounded down — tokens the caller receives.
#[inline(always)]
pub fn get_amount0_delta_signed(
    sqrt_price_a: &SqrtPriceX96,
    sqrt_price_b: &SqrtPriceX96,
    liquidity: i128,
) -> Result<I256, SqrtPriceMathError> {
    let result = if liquidity.is_negative() {
        I256::from(
            get_amount0_delta(
                sqrt_price_a,
                sqrt_price_b,
                Liquidity::new(liquidity.unsigned_abs()),
                false,
            )?
            .0,
        )
    } else {
        -I256::from(
            get_amount0_delta(
                sqrt_price_a,
                sqrt_price_b,
                Liquidity::new(liquidity as u128),
                true,
            )?
            .0,
        )
    };
    Ok(result)
}

/// Computes token0 delta from a cached `liquidity * (sqrt_b - sqrt_a)` product.
///
/// Formula:
///
/// ```text
/// amount0 = liquidity_delta_sqrt * Q96 / (sqrt_a * sqrt_b)
/// ```
///
/// The numerator fits in U384: the cached product is at most 288 bits, and the
/// Q96 scale factor adds 96 bits.
#[inline]
pub(crate) fn get_amount0_delta_with_liquidity_delta(
    sqrt_price_a: &SqrtPriceX96,
    sqrt_price_b: &SqrtPriceX96,
    liquidity_delta_sqrt: U288,
    round_up: bool,
) -> Result<U256, SqrtPriceMathError> {
    let numerator: U384 = U384::from(liquidity_delta_sqrt) << 96;

    let sqrt_a = sqrt_price_a.as_u256();
    let sqrt_b = sqrt_price_b.as_u256();

    // Each sqrt price is a validated protocol value below `uint160`.
    //
    // 160 + 160 = 320 bits max.
    let denominator = U384::from(sqrt_a) * U384::from(sqrt_b);

    let result = if round_up {
        numerator.div_ceil(denominator)
    } else {
        numerator / denominator
    };

    if result.bit_len() > 256 {
        return Err(SqrtPriceMathError::Overflow);
    }

    Ok(U256::from(result))
}

/// Computes token1 required to cover a liquidity position between two sqrt prices.
///
/// Returns `(amount1, liquidity_delta_sqrt)`.
///
/// `liquidity_delta_sqrt` is the unscaled intermediate
/// `liquidity * (sqrt_b - sqrt_a)`. Returning it gives callers a stable cache
/// key for the shared range product and avoids recalculating it when they also
/// need token0 delta.
///
/// Formula:
///
/// ```text
/// amount1 = liquidity * (sqrt_b - sqrt_a) / Q96
/// ```
///
/// Since Q96 is a power of two, this is a multiply plus a right shift. The
/// implementation keeps the common product in U256 and widens only when needed.
#[inline]
pub fn get_amount1_delta(
    sqrt_price_a: &SqrtPriceX96,
    sqrt_price_b: &SqrtPriceX96,
    liquidity: Liquidity,
    round_up: bool,
) -> Result<(U256, U288), SqrtPriceMathError> {
    let (sqrt_a, sqrt_b) = sqrt_price_a.sort(sqrt_price_b);

    let sqrt_a = sqrt_a.as_u256();
    let sqrt_b = sqrt_b.as_u256();

    let diff = sqrt_b - sqrt_a;

    if diff.is_zero() {
        return Ok((U256::ZERO, U288::ZERO));
    }

    let liquidity_128 = U128::from(liquidity.value());
    let diff_160 = diff.to::<U160>();
    let liquidity_delta_sqrt: U288 = liquidity_128.widening_mul(diff_160);

    let quotient = liquidity_delta_sqrt >> 96;
    let remainder_mask: U288 = (U288::ONE << 96) - U288::ONE;

    let result = if round_up && !(liquidity_delta_sqrt & remainder_mask).is_zero() {
        quotient + U288::ONE
    } else {
        quotient
    };

    Ok((U256::from(result), liquidity_delta_sqrt))
}

/// Returns the signed amount1 delta for a given liquidity delta and price range.
///
/// Uses the same caller-balance sign convention as [`get_amount0_delta_signed`].
#[inline(always)]
pub fn get_amount1_delta_signed(
    sqrt_price_a: &SqrtPriceX96,
    sqrt_price_b: &SqrtPriceX96,
    liquidity: i128,
) -> Result<I256, SqrtPriceMathError> {
    let result = if liquidity.is_negative() {
        I256::from(
            get_amount1_delta(
                sqrt_price_a,
                sqrt_price_b,
                Liquidity::new(liquidity.unsigned_abs()),
                false,
            )?
            .0,
        )
    } else {
        -I256::from(
            get_amount1_delta(
                sqrt_price_a,
                sqrt_price_b,
                Liquidity::new(liquidity as u128),
                true,
            )?
            .0,
        )
    };
    Ok(result)
}

/// Computes token1 delta from a cached `liquidity * (sqrt_b - sqrt_a)` product.
///
/// This is the same operation as `get_amount1_delta`, but starts from the
/// caller-provided shared product so amount0 and amount1 calculations can reuse
/// the same range intermediate.
pub(crate) fn get_amount1_delta_with_liquidity_delta(
    liquidity_delta_sqrt: U288,
    round_up: bool,
) -> U256 {
    let remainder_mask: U288 = (U288::ONE << 96) - U288::ONE;
    let quotient = liquidity_delta_sqrt >> 96;

    let result = if round_up && !(liquidity_delta_sqrt & remainder_mask).is_zero() {
        quotient + U288::ONE
    } else {
        quotient
    };

    U256::from(result)
}

/// Computes the next sqrt price for an exact-input swap.
///
/// zero_for_one = true:
///     input token0, price moves down
///
/// zero_for_one = false:
///     input token1, price moves up
///
/// Zero amounts are unrepresentable; callers must short-circuit them.
#[inline(always)]
pub fn get_next_sqrt_price_from_input(
    sqrt_price: &SqrtPriceX96,
    liquidity: &NonZeroLiquidity,
    amount_in: &NonZeroU256,
    zero_for_one: bool,
) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    if zero_for_one {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_price, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_price, liquidity, amount_in, true)
    }
}

/// Computes the next sqrt price for an exact-output swap.
///
/// zero_for_one = true:
///     output token1, price moves down
///
/// zero_for_one = false:
///     output token0, price moves up
///
/// Zero amounts are unrepresentable; callers must short-circuit them.
#[inline(always)]
pub fn get_next_sqrt_price_from_output(
    sqrt_price: &SqrtPriceX96,
    liquidity: &NonZeroLiquidity,
    amount_out: &NonZeroU256,
    zero_for_one: bool,
) -> Result<SqrtPriceX96, SqrtPriceMathError> {
    if zero_for_one {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_price, liquidity, amount_out, false)
    } else {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_price, liquidity, amount_out, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use ruint::aliases::U512;

    fn q96() -> U256 {
        U256::ONE << Q96_SHIFT
    }

    fn u256(s: &str) -> U256 {
        U256::from_str_radix(s, 10).unwrap()
    }

    fn ether(n: u128) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    fn sqrt_price(value: U256) -> SqrtPriceX96 {
        SqrtPriceX96::from_u256(value).unwrap()
    }

    fn liquidity(value: u128) -> Liquidity {
        Liquidity::new(value)
    }

    fn non_zero_liquidity(value: u128) -> NonZeroLiquidity {
        NonZeroLiquidity::new(value).unwrap()
    }

    fn amount(value: U256) -> NonZeroU256 {
        NonZeroU256::new(value).unwrap()
    }

    /// Solidity `getNextSqrtPriceFromAmount0RoundingUp` evaluated in U512,
    /// including the overflow fallback, followed by the protocol range check.
    fn ref_next_sqrt_price_from_amount0(
        sqrt_price: U256,
        liquidity: u128,
        amount: U256,
        add: bool,
    ) -> Option<SqrtPriceX96> {
        let max = U512::from(U256::MAX);
        let sqrt_p = U512::from(sqrt_price);
        let numerator1 = U512::from(liquidity) << Q96_SHIFT;
        let product = U512::from(amount) * sqrt_p;

        let result = if add {
            let denominator = numerator1 + product;
            if product <= max && denominator <= max {
                (numerator1 * sqrt_p).div_ceil(denominator)
            } else {
                let denominator = numerator1 / sqrt_p + U512::from(amount);
                if denominator > max {
                    return None;
                }
                numerator1.div_ceil(denominator)
            }
        } else {
            if product > max || numerator1 <= product {
                return None;
            }
            (numerator1 * sqrt_p).div_ceil(numerator1 - product)
        };

        if result > max {
            return None;
        }
        SqrtPriceX96::from_u256(result.to::<U256>())
    }

    /// Solidity `getNextSqrtPriceFromAmount1RoundingDown` evaluated in U512,
    /// followed by the protocol range check.
    fn ref_next_sqrt_price_from_amount1(
        sqrt_price: U256,
        liquidity: u128,
        amount: U256,
        add: bool,
    ) -> Option<SqrtPriceX96> {
        let sqrt_p = U512::from(sqrt_price);
        let numerator = U512::from(amount) << Q96_SHIFT;
        let liquidity = U512::from(liquidity);

        let result = if add {
            sqrt_p + numerator / liquidity
        } else {
            let quotient = numerator.div_ceil(liquidity);
            if sqrt_p <= quotient {
                return None;
            }
            sqrt_p - quotient
        };

        if result > U512::from(U256::MAX) {
            return None;
        }
        SqrtPriceX96::from_u256(result.to::<U256>())
    }

    fn ref_amount0_delta(
        sqrt_price_a: &SqrtPriceX96,
        sqrt_price_b: &SqrtPriceX96,
        liquidity: Liquidity,
        round_up: bool,
    ) -> U256 {
        let (sqrt_a, sqrt_b) = sqrt_price_a.sort(sqrt_price_b);
        let diff = sqrt_b.as_u256() - sqrt_a.as_u256();

        if diff.is_zero() {
            return U256::ZERO;
        }

        let numerator =
            U512::from(liquidity.as_u256()) * U512::from(diff) * (U512::ONE << Q96_SHIFT);
        let denominator = U512::from(sqrt_a.as_u256()) * U512::from(sqrt_b.as_u256());
        let result = if round_up {
            numerator.div_ceil(denominator)
        } else {
            numerator / denominator
        };

        result.to::<U256>()
    }

    fn ref_amount1_delta(
        sqrt_price_a: &SqrtPriceX96,
        sqrt_price_b: &SqrtPriceX96,
        liquidity: Liquidity,
        round_up: bool,
    ) -> U256 {
        let (sqrt_a, sqrt_b) = sqrt_price_a.sort(sqrt_price_b);
        let product =
            U512::from(liquidity.as_u256()) * U512::from(sqrt_b.as_u256() - sqrt_a.as_u256());
        let denominator = U512::ONE << Q96_SHIFT;
        let result = if round_up {
            product.div_ceil(denominator)
        } else {
            product / denominator
        };

        result.to::<U256>()
    }

    /// Sqrt prices spread across every bit length of the protocol range.
    fn arb_sqrt_price() -> impl Strategy<Value = SqrtPriceX96> {
        (33u32..=160, any::<[u64; 4]>())
            .prop_filter_map("sqrt price inside the protocol range", |(bits, limbs)| {
                SqrtPriceX96::from_u256(U256::from_limbs(limbs) >> (256 - bits))
            })
    }

    /// Non-zero liquidity with a uniformly chosen bit length.
    fn arb_liquidity() -> impl Strategy<Value = u128> {
        (1u32..=128, any::<u128>())
            .prop_map(|(bits, raw)| (raw >> (128 - bits)) | (1 << (bits - 1)))
    }

    /// Non-zero amounts with a uniformly chosen bit length, so every
    /// fast/wide/fallback branch is reached.
    fn arb_amount() -> impl Strategy<Value = U256> {
        (1usize..=256, any::<[u64; 4]>()).prop_map(|(bits, limbs)| {
            (U256::from_limbs(limbs) >> (256 - bits)) | (U256::ONE << (bits - 1))
        })
    }

    // ---------------------------------------------------------
    // Next sqrt price: Uniswap v3-core `SqrtPriceMath.spec.ts` vectors
    // ---------------------------------------------------------

    #[test]
    fn next_sqrt_price_from_input_matches_uniswap_vectors() {
        let price = sqrt_price(q96());
        let liquidity = non_zero_liquidity(ether(1).to());
        let amount_in = amount(ether(1) / U256::from(10u64));

        assert_eq!(
            get_next_sqrt_price_from_input(&price, &liquidity, &amount_in, false).unwrap(),
            sqrt_price(u256("87150978765690771352898345369"))
        );
        assert_eq!(
            get_next_sqrt_price_from_input(&price, &liquidity, &amount_in, true).unwrap(),
            sqrt_price(u256("72025602285694852357767227579"))
        );
    }

    #[test]
    fn next_sqrt_price_from_input_handles_amount_above_uint96() {
        assert_eq!(
            get_next_sqrt_price_from_input(
                &sqrt_price(q96()),
                &non_zero_liquidity(ether(10).to()),
                &amount(U256::ONE << 100),
                true,
            )
            .unwrap(),
            sqrt_price(u256("624999999995069620"))
        );
    }

    #[test]
    fn next_sqrt_price_from_input_rejects_fallback_result_below_min_price() {
        // amount * sqrtP overflows U256, so the Solidity fallback runs and
        // yields sqrtP = 1, which Solidity returns but is outside the domain.
        let err = get_next_sqrt_price_from_input(
            &sqrt_price(q96()),
            &non_zero_liquidity(1),
            &amount(U256::MAX >> 1),
            true,
        )
        .unwrap_err();

        assert_eq!(err, SqrtPriceMathError::InvalidSqrtPrice);
    }

    #[test]
    fn next_sqrt_price_from_output_matches_uniswap_vectors() {
        let price = sqrt_price(q96());
        let liquidity = non_zero_liquidity(ether(1).to());
        let amount_out = amount(ether(1) / U256::from(10u64));

        assert_eq!(
            get_next_sqrt_price_from_output(&price, &liquidity, &amount_out, true).unwrap(),
            sqrt_price(u256("71305346262837903834189555302"))
        );
        assert_eq!(
            get_next_sqrt_price_from_output(&price, &liquidity, &amount_out, false).unwrap(),
            sqrt_price(u256("88031291682515930659493278152"))
        );
    }

    #[test]
    fn next_sqrt_price_from_output_rejects_amounts_at_or_above_virtual_reserves() {
        let price = sqrt_price(U256::ONE << 104);
        let liquidity = non_zero_liquidity(1024);
        let output = |amount_out: u64, zero_for_one: bool| {
            get_next_sqrt_price_from_output(
                &price,
                &liquidity,
                &amount(U256::from(amount_out)),
                zero_for_one,
            )
        };

        // Virtual token0 reserves are exactly 4.
        assert_eq!(
            output(4, false).unwrap_err(),
            SqrtPriceMathError::InsufficientToken0Reserves
        );
        assert_eq!(
            output(5, false).unwrap_err(),
            SqrtPriceMathError::InsufficientToken0Reserves
        );

        // Virtual token1 reserves are exactly 262144.
        assert_eq!(
            output(262_144, true).unwrap_err(),
            SqrtPriceMathError::InsufficientToken1Reserves
        );
        assert_eq!(
            output(262_145, true).unwrap_err(),
            SqrtPriceMathError::InsufficientToken1Reserves
        );
        assert_eq!(
            output(262_143, true).unwrap(),
            sqrt_price(u256("77371252455336267181195264"))
        );
    }

    // ---------------------------------------------------------
    // Next sqrt price: branch coverage against the wide reference
    // ---------------------------------------------------------

    #[test]
    fn amount0_next_price_matches_reference_on_every_branch() {
        let max = SqrtPriceX96::MAX.as_u256();
        let cases = [
            // bits(amount) + bits(sqrtP) <= 255: unchecked fast path.
            (q96(), 1_000_000u128, U256::from(1_000u64)),
            // == 256: product fits, denominator addition is checked.
            (max, u128::MAX, U256::ONE << 95),
            // > 256 while the product still fits: overflowing_mul path.
            (max, u128::MAX, U256::ONE << 96),
            // Product overflows: Solidity fallback formula.
            (max, 1, U256::MAX),
        ];

        for (sqrt_p, liquidity_raw, amount_raw) in cases {
            for add in [true, false] {
                assert_eq!(
                    get_next_sqrt_price_from_amount0_rounding_up(
                        &sqrt_price(sqrt_p),
                        &non_zero_liquidity(liquidity_raw),
                        &amount(amount_raw),
                        add,
                    )
                    .ok(),
                    ref_next_sqrt_price_from_amount0(sqrt_p, liquidity_raw, amount_raw, add),
                    "sqrt_p={sqrt_p} liquidity={liquidity_raw} amount={amount_raw} add={add}"
                );
            }
        }
    }

    #[test]
    fn amount1_wide_path_matches_reference() {
        // amount > uint160 forces the U352 path; the quotient (~2^129) still
        // fits, so both directions must succeed.
        let sqrt_p = U256::ONE << 140;
        let amount_raw = (U256::ONE << 161) + U256::from(12_345u64);

        for add in [true, false] {
            let expected = ref_next_sqrt_price_from_amount1(sqrt_p, u128::MAX, amount_raw, add);

            assert!(expected.is_some());
            assert_eq!(
                get_next_sqrt_price_from_amount1_rounding_down(
                    &sqrt_price(sqrt_p),
                    &non_zero_liquidity(u128::MAX),
                    &amount(amount_raw),
                    add,
                )
                .ok(),
                expected
            );
        }
    }

    #[test]
    fn amount1_add_reports_price_overflow_for_oversized_quotient() {
        let err = get_next_sqrt_price_from_amount1_rounding_down(
            &sqrt_price(q96()),
            &non_zero_liquidity(1),
            &amount(U256::ONE << 200),
            true,
        )
        .unwrap_err();

        assert_eq!(err, SqrtPriceMathError::PriceOverflow);
    }

    #[test]
    fn amount1_remove_reports_insufficient_reserves_for_oversized_quotient() {
        let err = get_next_sqrt_price_from_amount1_rounding_down(
            &sqrt_price(q96()),
            &non_zero_liquidity(1),
            &amount(U256::ONE << 200),
            false,
        )
        .unwrap_err();

        assert_eq!(err, SqrtPriceMathError::InsufficientToken1Reserves);
    }

    proptest! {
        #[test]
        fn next_sqrt_price_matches_wide_reference(
            price in arb_sqrt_price(),
            liquidity_raw in arb_liquidity(),
            amount_raw in arb_amount(),
            add in any::<bool>(),
        ) {
            let sqrt_p = price.as_u256();
            let liquidity = non_zero_liquidity(liquidity_raw);
            let amount = amount(amount_raw);

            prop_assert_eq!(
                get_next_sqrt_price_from_amount0_rounding_up(&price, &liquidity, &amount, add).ok(),
                ref_next_sqrt_price_from_amount0(sqrt_p, liquidity_raw, amount_raw, add)
            );
            prop_assert_eq!(
                get_next_sqrt_price_from_amount1_rounding_down(&price, &liquidity, &amount, add).ok(),
                ref_next_sqrt_price_from_amount1(sqrt_p, liquidity_raw, amount_raw, add)
            );
        }
    }

    // ---------------------------------------------------------
    // Amount deltas
    // ---------------------------------------------------------

    #[test]
    fn amount_deltas_match_uniswap_vectors() {
        // Price 1 -> 1.21 with 1e18 liquidity.
        let sqrt_a = sqrt_price(q96());
        let sqrt_b = sqrt_price(u256("87150978765690771352898345369"));
        let liquidity = liquidity(ether(1).to());

        let amount0 = |round_up| {
            get_amount0_delta(&sqrt_a, &sqrt_b, liquidity, round_up)
                .unwrap()
                .0
        };
        let amount1 = |round_up| {
            get_amount1_delta(&sqrt_a, &sqrt_b, liquidity, round_up)
                .unwrap()
                .0
        };

        assert_eq!(amount0(true), u256("90909090909090910"));
        assert_eq!(amount0(false), u256("90909090909090909"));
        assert_eq!(amount1(true), u256("100000000000000000"));
        assert_eq!(amount1(false), u256("99999999999999999"));
    }

    #[test]
    fn amount_deltas_are_zero_for_equal_prices() {
        let price = sqrt_price(q96());

        assert_eq!(
            get_amount0_delta(&price, &price, Liquidity::MAX, true).unwrap(),
            (U256::ZERO, U288::ZERO)
        );
        assert_eq!(
            get_amount1_delta(&price, &price, Liquidity::MAX, true).unwrap(),
            (U256::ZERO, U288::ZERO)
        );
    }

    #[test]
    fn amount0_delta_applies_q96_scale_before_dividing() {
        let sqrt_a = sqrt_price(q96());
        let sqrt_b = sqrt_price(q96() * U256::from(2u64));
        let liquidity = liquidity(1_000_000);

        for round_up in [false, true] {
            assert_eq!(
                get_amount0_delta(&sqrt_a, &sqrt_b, liquidity, round_up)
                    .unwrap()
                    .0,
                U256::from(500_000u64)
            );
        }
    }

    #[test]
    fn amount0_delta_rounds_up_when_fractional() {
        let sqrt_a = sqrt_price(q96());
        let sqrt_b = sqrt_price(q96() + U256::ONE);
        let liquidity = liquidity(1);

        assert_eq!(
            get_amount0_delta(&sqrt_a, &sqrt_b, liquidity, false)
                .unwrap()
                .0,
            U256::ZERO
        );
        assert_eq!(
            get_amount0_delta(&sqrt_a, &sqrt_b, liquidity, true)
                .unwrap()
                .0,
            U256::ONE
        );
    }

    #[test]
    fn amount_delta_helpers_match_public_paths() {
        let sqrt_a = sqrt_price(q96() / U256::from(2u64));
        let sqrt_b = sqrt_price(q96() * U256::from(3u64));
        let liquidity = liquidity(u128::MAX);

        let diff = (sqrt_b.as_u256() - sqrt_a.as_u256()).to::<U160>();
        let liquidity_delta_sqrt: U288 = U128::from(liquidity.value()).widening_mul(diff);

        for round_up in [false, true] {
            let (amount0, amount0_liquidity_delta_sqrt) =
                get_amount0_delta(&sqrt_a, &sqrt_b, liquidity, round_up).unwrap();
            let (amount1, amount1_liquidity_delta_sqrt) =
                get_amount1_delta(&sqrt_a, &sqrt_b, liquidity, round_up).unwrap();

            assert_eq!(amount0_liquidity_delta_sqrt, liquidity_delta_sqrt);
            assert_eq!(amount1_liquidity_delta_sqrt, liquidity_delta_sqrt);
            assert_eq!(
                get_amount0_delta_with_liquidity_delta(
                    &sqrt_a,
                    &sqrt_b,
                    liquidity_delta_sqrt,
                    round_up,
                )
                .unwrap(),
                amount0
            );
            assert_eq!(
                get_amount1_delta_with_liquidity_delta(liquidity_delta_sqrt, round_up),
                amount1
            );
        }
    }

    #[test]
    fn amount_deltas_match_wide_reference() {
        let cases = [
            (q96(), q96() + U256::ONE, 1u128),
            (q96(), q96() + U256::from(1_000_000u64), 1_000_000u128),
            (
                q96() / U256::from(2u64),
                q96() * U256::from(2u64),
                123_456_789u128,
            ),
            (U256::ONE << 80, U256::ONE << 120, 1u128 << 100),
            (
                SqrtPriceX96::MAX.as_u256() - U256::from(1_000_000u64),
                SqrtPriceX96::MAX.as_u256(),
                u128::MAX,
            ),
            (
                SqrtPriceX96::MIN.as_u256(),
                SqrtPriceX96::MAX.as_u256(),
                u128::MAX,
            ),
        ];

        for (sqrt_a, sqrt_b, liquidity_raw) in cases {
            let sqrt_a = sqrt_price(sqrt_a);
            let sqrt_b = sqrt_price(sqrt_b);
            let liquidity = liquidity(liquidity_raw);

            for round_up in [false, true] {
                // Argument order must not matter.
                for (lhs, rhs) in [(&sqrt_a, &sqrt_b), (&sqrt_b, &sqrt_a)] {
                    assert_eq!(
                        get_amount0_delta(lhs, rhs, liquidity, round_up).unwrap().0,
                        ref_amount0_delta(lhs, rhs, liquidity, round_up)
                    );
                    assert_eq!(
                        get_amount1_delta(lhs, rhs, liquidity, round_up).unwrap().0,
                        ref_amount1_delta(lhs, rhs, liquidity, round_up)
                    );
                }
            }
        }
    }

    #[test]
    fn signed_amount_deltas_use_caller_balance_sign_and_rounding() {
        // A one-unit price step with liquidity 1 gives 0 when rounded down and
        // 1 when rounded up for both tokens.
        let sqrt_a = sqrt_price(q96());
        let sqrt_b = sqrt_price(q96() + U256::ONE);
        let minus_one = -I256::from(U256::ONE);

        assert_eq!(
            get_amount0_delta_signed(&sqrt_a, &sqrt_b, 1).unwrap(),
            minus_one
        );
        assert_eq!(
            get_amount1_delta_signed(&sqrt_a, &sqrt_b, 1).unwrap(),
            minus_one
        );
        assert_eq!(
            get_amount0_delta_signed(&sqrt_a, &sqrt_b, -1).unwrap(),
            I256::ZERO
        );
        assert_eq!(
            get_amount1_delta_signed(&sqrt_a, &sqrt_b, -1).unwrap(),
            I256::ZERO
        );
        assert_eq!(
            get_amount0_delta_signed(&sqrt_a, &sqrt_b, 0).unwrap(),
            I256::ZERO
        );
    }

    #[test]
    fn signed_amount_deltas_handle_i128_min() {
        let sqrt_a = sqrt_price(SqrtPriceX96::MIN.as_u256());
        let sqrt_b = sqrt_price(SqrtPriceX96::MAX.as_u256());
        let liquidity = liquidity(1u128 << 127);

        assert_eq!(
            get_amount0_delta_signed(&sqrt_a, &sqrt_b, i128::MIN).unwrap(),
            I256::from(
                get_amount0_delta(&sqrt_a, &sqrt_b, liquidity, false)
                    .unwrap()
                    .0
            )
        );
        assert_eq!(
            get_amount1_delta_signed(&sqrt_a, &sqrt_b, i128::MIN).unwrap(),
            I256::from(
                get_amount1_delta(&sqrt_a, &sqrt_b, liquidity, false)
                    .unwrap()
                    .0
            )
        );
    }
}
