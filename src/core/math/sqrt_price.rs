//! Optimized port of Uniswap V4 `SqrtPriceMath`.
//!
//! Design goals:
//! - Keep Uniswap-compatible rounding semantics.
//! - Avoid Solidity-style overflow checks that are expensive in Rust.
//! - Avoid generic FullMath paths when the expression has a cheaper fixed form.
//! - Reject values outside the real Uniswap V4 domain instead of silently
//!   truncating left shifts.

use crate::core::math::full::{MathError, mul_q96_div, mul_q96_div_rounding_up};
use ruint::aliases::{U256, U512};

/// Q96 = 2^96.
const Q96_SHIFT: u32 = 96;

/// Maximum uint128: Uniswap V4 liquidity type.
const MAX_UINT128: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

/// Maximum uint160: Uniswap V4 sqrt price type.
const MAX_UINT160: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0xFFFF_FFFF, 0]);

/// Low 96-bit mask for U256.
const Q96_MASK_U256: U256 = U256::from_limbs([u64::MAX, 0xFFFF_FFFF, 0, 0]);

/// Low 96-bit mask for U512.
const Q96_MASK_U512: U512 = U512::from_limbs([u64::MAX, 0xFFFF_FFFF, 0, 0, 0, 0, 0, 0]);

// ─── Error type ───────────────────────────────────────────────────────────────

/// Backward-compatible name for the crate-wide compact error code.
pub type SqrtPriceMathError = crate::Error;

// ─── Basic helpers ────────────────────────────────────────────────────────────

#[inline(always)]
fn widen_u256(v: U256) -> U512 {
    let [a, b, c, d] = v.into_limbs();
    U512::from_limbs([a, b, c, d, 0, 0, 0, 0])
}

#[inline(always)]
fn narrow_u512_to_u256(v: U512) -> Result<U256, MathError> {
    let [a, b, c, d, e, f, g, h] = v.into_limbs();

    if (e | f | g | h) != 0 {
        return Err(MathError::Overflow);
    }

    Ok(U256::from_limbs([a, b, c, d]))
}

#[inline(always)]
fn sort_sqrt_ratios(a: U256, b: U256) -> (U256, U256) {
    if a > b { (b, a) } else { (a, b) }
}

#[inline(always)]
fn validate_sqrt_price(sqrt_p_x96: U256) -> Result<(), SqrtPriceMathError> {
    if sqrt_p_x96.is_zero() {
        return Err(SqrtPriceMathError::ZeroPrice);
    }

    if sqrt_p_x96 > MAX_UINT160 {
        return Err(SqrtPriceMathError::PriceOverflow);
    }

    Ok(())
}

#[inline(always)]
fn validate_nonzero_liquidity(liquidity: U256) -> Result<(), SqrtPriceMathError> {
    if liquidity.is_zero() {
        return Err(SqrtPriceMathError::ZeroLiquidity);
    }

    if liquidity > MAX_UINT128 {
        return Err(SqrtPriceMathError::PriceOverflow);
    }

    Ok(())
}

#[inline(always)]
fn validate_liquidity_allow_zero(liquidity: U256) -> Result<(), SqrtPriceMathError> {
    if liquidity > MAX_UINT128 {
        return Err(SqrtPriceMathError::PriceOverflow);
    }

    Ok(())
}

#[inline(always)]
fn validate_price_and_liquidity(
    sqrt_p_x96: U256,
    liquidity: U256,
) -> Result<(), SqrtPriceMathError> {
    validate_sqrt_price(sqrt_p_x96)?;
    validate_nonzero_liquidity(liquidity)?;
    Ok(())
}

#[inline(always)]
fn validate_delta_inputs(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: U256,
) -> Result<(), SqrtPriceMathError> {
    if sqrt_a.is_zero() {
        return Err(SqrtPriceMathError::ZeroPrice);
    }

    if sqrt_b > MAX_UINT160 {
        return Err(SqrtPriceMathError::PriceOverflow);
    }

    validate_liquidity_allow_zero(liquidity)?;

    Ok(())
}

#[inline(always)]
fn liquidity_q96_unchecked(liquidity: U256) -> U256 {
    debug_assert!(liquidity <= MAX_UINT128);
    liquidity << Q96_SHIFT
}

// ─── Division helpers ─────────────────────────────────────────────────────────

#[inline(always)]
fn div_rounding_up_u256_nonzero(a: U256, b: U256) -> Result<U256, MathError> {
    debug_assert!(!b.is_zero());

    let (q, r) = a.div_rem(b);

    if r.is_zero() {
        Ok(q)
    } else {
        q.checked_add(U256::ONE).ok_or(MathError::Overflow)
    }
}

#[inline(always)]
fn div_u512_by_u256_rounding_up(numerator: U512, denominator: U256) -> Result<U256, MathError> {
    debug_assert!(!denominator.is_zero());

    let (q512, r512) = numerator.div_rem(widen_u256(denominator));
    let q = narrow_u512_to_u256(q512)?;

    if r512.is_zero() {
        Ok(q)
    } else {
        q.checked_add(U256::ONE).ok_or(MathError::Overflow)
    }
}

#[inline(always)]
fn div_u512_by_u512_floor(numerator: U512, denominator: U512) -> Result<U256, MathError> {
    debug_assert!(!denominator.is_zero());

    let q512 = numerator / denominator;
    narrow_u512_to_u256(q512)
}

#[inline(always)]
fn div_u512_by_u512_rounding_up(numerator: U512, denominator: U512) -> Result<U256, MathError> {
    debug_assert!(!denominator.is_zero());

    let (q512, r512) = numerator.div_rem(denominator);
    let q = narrow_u512_to_u256(q512)?;

    if r512.is_zero() {
        Ok(q)
    } else {
        q.checked_add(U256::ONE).ok_or(MathError::Overflow)
    }
}

/// Computes ceil((liquidity * sqrt_p_x96 * Q96) / denominator).
///
/// This replaces:
///
/// ```text
/// mul_div_rounding_up(liquidity << 96, sqrt_p_x96, denominator)
/// ```
///
/// in the token0 next-price path.
///
/// Why this is better:
/// - `liquidity << 96` is a known Q96 form.
/// - `liquidity * sqrt_p_x96 * Q96` fits in U512 under real Uniswap bounds.
/// - We avoid the generic FullMath modular-inverse path.
#[inline(always)]
fn liquidity_sqrt_q96_div_rounding_up(
    liquidity: U256,
    sqrt_p_x96: U256,
    denominator: U256,
) -> Result<U256, MathError> {
    debug_assert!(liquidity <= MAX_UINT128);
    debug_assert!(sqrt_p_x96 <= MAX_UINT160);
    debug_assert!(!denominator.is_zero());

    let numerator = liquidity.widening_mul(sqrt_p_x96) << Q96_SHIFT;
    div_u512_by_u256_rounding_up(numerator, denominator)
}

// ─── Q96 multiply / shift helpers ─────────────────────────────────────────────

#[inline(always)]
fn mul_shift_right_96(a: U256, b: U256) -> Result<U256, MathError> {
    let (product_256, overflow) = a.overflowing_mul(b);

    if !overflow {
        return Ok(product_256 >> Q96_SHIFT);
    }

    let product_512: U512 = a.widening_mul(b);
    narrow_u512_to_u256(product_512 >> Q96_SHIFT)
}

#[inline(always)]
fn mul_shift_right_96_rounding_up(a: U256, b: U256) -> Result<U256, MathError> {
    let (product_256, overflow) = a.overflowing_mul(b);

    if !overflow {
        let quotient = product_256 >> Q96_SHIFT;
        let has_remainder = !(product_256 & Q96_MASK_U256).is_zero();

        return if has_remainder {
            quotient.checked_add(U256::ONE).ok_or(MathError::Overflow)
        } else {
            Ok(quotient)
        };
    }

    let product_512: U512 = a.widening_mul(b);

    let quotient = narrow_u512_to_u256(product_512 >> Q96_SHIFT)?;
    let has_remainder = !(product_512 & Q96_MASK_U512).is_zero();

    if has_remainder {
        quotient.checked_add(U256::ONE).ok_or(MathError::Overflow)
    } else {
        Ok(quotient)
    }
}

// ─── Core price-move functions ────────────────────────────────────────────────

/// Compute the next sqrt price after adding or removing token0.
///
/// Formula:
///
/// add = true:
///     ceil(liq * Q96 * sqrtP / (liq * Q96 + amount * sqrtP))
///
/// add = false:
///     ceil(liq * Q96 * sqrtP / (liq * Q96 - amount * sqrtP))
#[inline]
pub fn get_next_sqrt_price_from_amount0_rounding_up(
    sqrt_p_x96: U256,
    liquidity: U256,
    amount: U256,
    add: bool,
) -> Result<U256, SqrtPriceMathError> {
    validate_price_and_liquidity(sqrt_p_x96, liquidity)?;

    if amount.is_zero() {
        return Ok(sqrt_p_x96);
    }

    let numerator1 = liquidity_q96_unchecked(liquidity);

    if add {
        // Rust-native overflow check.
        //
        // This replaces Solidity's:
        //
        //     product / amount == sqrtPX96
        //
        // Avoiding that U256 division is a major hot-path win.
        let (product, product_overflowed) = amount.overflowing_mul(sqrt_p_x96);

        if !product_overflowed {
            let (denominator, denominator_overflowed) = numerator1.overflowing_add(product);

            if !denominator_overflowed {
                let result =
                    liquidity_sqrt_q96_div_rounding_up(liquidity, sqrt_p_x96, denominator)?;

                if result > MAX_UINT160 {
                    return Err(SqrtPriceMathError::PriceOverflow);
                }

                return Ok(result);
            }
        }

        // Overflow-safe algebraic fallback:
        //
        //     ceil(liq * Q96 / (floor(liq * Q96 / sqrtP) + amount))
        //
        // This matches the Solidity fallback branch.
        let base = numerator1 / sqrt_p_x96;
        let denominator = base
            .checked_add(amount)
            .ok_or_else(|| SqrtPriceMathError::PriceOverflow)?;

        let result = div_rounding_up_u256_nonzero(numerator1, denominator)?;

        if result > MAX_UINT160 {
            return Err(SqrtPriceMathError::PriceOverflow);
        }

        Ok(result)
    } else {
        let (product, product_overflowed) = amount.overflowing_mul(sqrt_p_x96);

        if product_overflowed || numerator1 <= product {
            return Err(SqrtPriceMathError::InsufficientToken0Reserves);
        }

        let denominator = numerator1 - product;

        let result = liquidity_sqrt_q96_div_rounding_up(liquidity, sqrt_p_x96, denominator)?;

        if result > MAX_UINT160 {
            return Err(SqrtPriceMathError::PriceOverflow);
        }

        Ok(result)
    }
}

/// Compute the next sqrt price after adding or removing token1.
///
/// add = true:
///     sqrtP + floor(amount * Q96 / liquidity)
///
/// add = false:
///     sqrtP - ceil(amount * Q96 / liquidity)
#[inline]
pub fn get_next_sqrt_price_from_amount1_rounding_down(
    sqrt_p_x96: U256,
    liquidity: U256,
    amount: U256,
    add: bool,
) -> Result<U256, SqrtPriceMathError> {
    validate_price_and_liquidity(sqrt_p_x96, liquidity)?;

    if amount.is_zero() {
        return Ok(sqrt_p_x96);
    }

    if add {
        let quotient = if amount <= MAX_UINT160 {
            (amount << Q96_SHIFT) / liquidity
        } else {
            mul_q96_div(amount, liquidity)?
        };

        let result = sqrt_p_x96
            .checked_add(quotient)
            .ok_or_else(|| SqrtPriceMathError::PriceOverflow)?;

        if result > MAX_UINT160 {
            return Err(SqrtPriceMathError::PriceOverflow);
        }

        Ok(result)
    } else {
        let quotient = if amount <= MAX_UINT160 {
            div_rounding_up_u256_nonzero(amount << Q96_SHIFT, liquidity)?
        } else {
            mul_q96_div_rounding_up(amount, liquidity)?
        };

        if sqrt_p_x96 <= quotient {
            return Err(SqrtPriceMathError::PriceUnderflow);
        }

        Ok(sqrt_p_x96 - quotient)
    }
}

// ─── Amount0 delta helpers ────────────────────────────────────────────────────

#[inline(always)]
fn amount0_delta_wide(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: U256,
    diff: U256,
    round_up: bool,
) -> Result<U256, MathError> {
    debug_assert!(!sqrt_a.is_zero());
    debug_assert!(sqrt_a <= sqrt_b);
    debug_assert!(sqrt_b <= MAX_UINT160);
    debug_assert!(liquidity <= MAX_UINT128);
    debug_assert!(!diff.is_zero());
    debug_assert!(!liquidity.is_zero());

    // numerator = liquidity * diff * Q96
    //
    // Under real Uniswap bounds:
    //
    //     liquidity <= uint128
    //     diff      <= uint160
    //     Q96       = 2^96
    //
    // numerator fits in 384 bits.
    let product: U512 = liquidity.widening_mul(diff);

    // Defensive guard for non-Uniswap-domain values.
    if (product >> (512 - Q96_SHIFT)) != U512::ZERO {
        return Err(MathError::Overflow);
    }

    let numerator = product << Q96_SHIFT;

    // denominator = sqrt_a * sqrt_b
    //
    // Under real Uniswap bounds this is at most 320 bits.
    let denominator: U512 = sqrt_a.widening_mul(sqrt_b);

    if round_up {
        div_u512_by_u512_rounding_up(numerator, denominator)
    } else {
        div_u512_by_u512_floor(numerator, denominator)
    }
}

#[inline(always)]
fn amount0_delta_fast_or_wide(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: U256,
    round_up: bool,
) -> Result<U256, MathError> {
    let diff = sqrt_b - sqrt_a;

    if diff.is_zero() || liquidity.is_zero() {
        return Ok(U256::ZERO);
    }

    let numerator1 = liquidity_q96_unchecked(liquidity);

    // Try the exact original two-step path without FullMath.
    //
    // Original floor:
    //     floor(floor((liquidity << 96) * diff / sqrt_b) / sqrt_a)
    //
    // Original ceil:
    //     ceil(ceil((liquidity << 96) * diff / sqrt_b) / sqrt_a)
    //
    // If `(liquidity << 96) * diff` fits in U256, this path is much cheaper
    // than forcing a U512 division.
    let (numerator_256, overflowed) = numerator1.overflowing_mul(diff);

    if !overflowed {
        if round_up {
            let first = div_rounding_up_u256_nonzero(numerator_256, sqrt_b)?;
            return div_rounding_up_u256_nonzero(first, sqrt_a);
        }

        return Ok((numerator_256 / sqrt_b) / sqrt_a);
    }

    // Wide path:
    //
    // Use the mathematically equivalent single division:
    //
    //     liquidity * diff * Q96 / (sqrt_a * sqrt_b)
    //
    // This avoids generic FullMath's U512 modulo + modular inverse path.
    amount0_delta_wide(sqrt_a, sqrt_b, liquidity, diff, round_up)
}

// ─── Delta functions ──────────────────────────────────────────────────────────

/// Compute token0 delta.
///
/// Formula:
///
/// ```text
/// amount0 = liquidity * Q96 * (sqrt_b - sqrt_a) / (sqrt_a * sqrt_b)
/// ```
///
/// This implementation is hybrid:
/// - U256 fast path when `(liquidity << 96) * diff` fits.
/// - U512 single-division path only when the fast path overflows.
#[inline]
pub fn get_amount0_delta(
    sqrt_ratio_a_x96: U256,
    sqrt_ratio_b_x96: U256,
    liquidity: U256,
    round_up: bool,
) -> Result<U256, SqrtPriceMathError> {
    let (sqrt_a, sqrt_b) = sort_sqrt_ratios(sqrt_ratio_a_x96, sqrt_ratio_b_x96);

    validate_delta_inputs(sqrt_a, sqrt_b, liquidity)?;

    let result = amount0_delta_fast_or_wide(sqrt_a, sqrt_b, liquidity, round_up)?;

    Ok(result)
}

/// Compute token1 delta.
///
/// Formula:
///
/// ```text
/// amount1 = liquidity * (sqrt_b - sqrt_a) / Q96
/// ```
///
/// Since Q96 is a power of two, this is multiply + right shift.
/// The helper avoids U512 construction when the product fits in U256.
#[inline]
pub fn get_amount1_delta(
    sqrt_ratio_a_x96: U256,
    sqrt_ratio_b_x96: U256,
    liquidity: U256,
    round_up: bool,
) -> Result<U256, SqrtPriceMathError> {
    let (sqrt_a, sqrt_b) = sort_sqrt_ratios(sqrt_ratio_a_x96, sqrt_ratio_b_x96);

    validate_delta_inputs(sqrt_a, sqrt_b, liquidity)?;

    let diff = sqrt_b - sqrt_a;

    if diff.is_zero() || liquidity.is_zero() {
        return Ok(U256::ZERO);
    }

    let result = if round_up {
        mul_shift_right_96_rounding_up(liquidity, diff)?
    } else {
        mul_shift_right_96(liquidity, diff)?
    };

    Ok(result)
}

// ─── High-level helpers ───────────────────────────────────────────────────────

/// Given the input amount and direction, compute the next sqrt price.
///
/// zero_for_one = true:
///     input token0, price moves down
///
/// zero_for_one = false:
///     input token1, price moves up
#[inline(always)]
pub fn get_next_sqrt_price_from_input(
    sqrt_p_x96: U256,
    liquidity: U256,
    amount_in: U256,
    zero_for_one: bool,
) -> Result<U256, SqrtPriceMathError> {
    if zero_for_one {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_p_x96, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_p_x96, liquidity, amount_in, true)
    }
}

/// Given the output amount and direction, compute the next sqrt price.
///
/// zero_for_one = true:
///     output token1, price moves down
///
/// zero_for_one = false:
///     output token0, price moves up
#[inline(always)]
pub fn get_next_sqrt_price_from_output(
    sqrt_p_x96: U256,
    liquidity: U256,
    amount_out: U256,
    zero_for_one: bool,
) -> Result<U256, SqrtPriceMathError> {
    if zero_for_one {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_p_x96, liquidity, amount_out, false)
    } else {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_p_x96, liquidity, amount_out, false)
    }
}

// ─── Public internal arithmetic ───────────────────────────────────────────────

#[inline]
pub(crate) fn div_rounding_up(a: U256, b: U256) -> Result<U256, SqrtPriceMathError> {
    if b.is_zero() {
        return Err(SqrtPriceMathError::ZeroDenominator);
    }

    Ok(div_rounding_up_u256_nonzero(a, b)?)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::math::full::{
        mul_div, mul_div_rounding_up, mul_shift_right, mul_shift_right_rounding_up,
    };

    #[inline(always)]
    fn q96() -> U256 {
        U256::ONE << 96
    }

    #[inline(always)]
    fn ref_div_rounding_up(a: U256, b: U256) -> Result<U256, SqrtPriceMathError> {
        if b.is_zero() {
            return Err(SqrtPriceMathError::ZeroDenominator);
        }

        let (q, r) = a.div_rem(b);

        if r.is_zero() {
            Ok(q)
        } else {
            q.checked_add(U256::ONE)
                .ok_or(SqrtPriceMathError::PriceOverflow)
        }
    }

    // ─── Reference implementations ────────────────────────────────────────────

    /// Reference version of getAmount0Delta using the original two-step
    /// Uniswap-style formula.
    ///
    /// This intentionally does not use the optimized amount0 delta path.
    fn ref_amount0_delta(
        sqrt_ratio_a_x96: U256,
        sqrt_ratio_b_x96: U256,
        liquidity: U256,
        round_up: bool,
    ) -> Result<U256, SqrtPriceMathError> {
        let (sqrt_a, sqrt_b) = if sqrt_ratio_a_x96 > sqrt_ratio_b_x96 {
            (sqrt_ratio_b_x96, sqrt_ratio_a_x96)
        } else {
            (sqrt_ratio_a_x96, sqrt_ratio_b_x96)
        };

        if sqrt_a.is_zero() {
            return Err(SqrtPriceMathError::ZeroPrice);
        }

        let numerator1 = liquidity << 96;
        let numerator2 = sqrt_b - sqrt_a;

        let result = if round_up {
            ref_div_rounding_up(mul_div_rounding_up(numerator1, numerator2, sqrt_b)?, sqrt_a)?
        } else {
            mul_div(numerator1, numerator2, sqrt_b)? / sqrt_a
        };

        Ok(result)
    }

    /// Reference version of getAmount1Delta using the generic shift helper.
    fn ref_amount1_delta(
        sqrt_ratio_a_x96: U256,
        sqrt_ratio_b_x96: U256,
        liquidity: U256,
        round_up: bool,
    ) -> Result<U256, SqrtPriceMathError> {
        let (sqrt_a, sqrt_b) = if sqrt_ratio_a_x96 > sqrt_ratio_b_x96 {
            (sqrt_ratio_b_x96, sqrt_ratio_a_x96)
        } else {
            (sqrt_ratio_a_x96, sqrt_ratio_b_x96)
        };

        if sqrt_a.is_zero() {
            return Err(SqrtPriceMathError::ZeroPrice);
        }

        let diff = sqrt_b - sqrt_a;

        let result = if round_up {
            mul_shift_right_rounding_up(liquidity, diff, 96)?
        } else {
            mul_shift_right(liquidity, diff, 96)?
        };

        Ok(result)
    }

    /// Reference version of getNextSqrtPriceFromAmount0RoundingUp.
    ///
    /// This mirrors the Solidity-style branch structure instead of using the
    /// optimized Rust-native helpers.
    fn ref_next_sqrt_price_from_amount0_rounding_up(
        sqrt_p_x96: U256,
        liquidity: U256,
        amount: U256,
        add: bool,
    ) -> Result<U256, SqrtPriceMathError> {
        if sqrt_p_x96.is_zero() {
            return Err(SqrtPriceMathError::ZeroPrice);
        }

        if liquidity.is_zero() {
            return Err(SqrtPriceMathError::ZeroLiquidity);
        }

        if amount.is_zero() {
            return Ok(sqrt_p_x96);
        }

        let numerator1: U256 = liquidity << 96;

        if add {
            let product = amount.wrapping_mul(sqrt_p_x96);
            let product_overflowed = product / amount != sqrt_p_x96;

            if !product_overflowed {
                let denominator = numerator1.wrapping_add(product);

                if denominator >= numerator1 {
                    let result = mul_div_rounding_up(numerator1, sqrt_p_x96, denominator)?;

                    if result > MAX_UINT160 {
                        return Err(SqrtPriceMathError::PriceOverflow);
                    }

                    return Ok(result);
                }
            }

            let base = numerator1 / sqrt_p_x96;
            let denominator = base
                .checked_add(amount)
                .ok_or(SqrtPriceMathError::PriceOverflow)?;

            let result = ref_div_rounding_up(numerator1, denominator)?;

            if result > MAX_UINT160 {
                return Err(SqrtPriceMathError::PriceOverflow);
            }

            Ok(result)
        } else {
            let product = amount.wrapping_mul(sqrt_p_x96);
            let product_overflowed = product / amount != sqrt_p_x96;

            if product_overflowed || numerator1 <= product {
                return Err(SqrtPriceMathError::InsufficientToken0Reserves);
            }

            let denominator = numerator1 - product;
            let result = mul_div_rounding_up(numerator1, sqrt_p_x96, denominator)?;

            if result > MAX_UINT160 {
                return Err(SqrtPriceMathError::PriceOverflow);
            }

            Ok(result)
        }
    }

    /// Reference version of getNextSqrtPriceFromAmount1RoundingDown.
    fn ref_next_sqrt_price_from_amount1_rounding_down(
        sqrt_p_x96: U256,
        liquidity: U256,
        amount: U256,
        add: bool,
    ) -> Result<U256, SqrtPriceMathError> {
        if sqrt_p_x96.is_zero() {
            return Err(SqrtPriceMathError::ZeroPrice);
        }

        if liquidity.is_zero() {
            return Err(SqrtPriceMathError::ZeroLiquidity);
        }

        if amount.is_zero() {
            return Ok(sqrt_p_x96);
        }

        if add {
            let quotient = if amount <= MAX_UINT160 {
                (amount << 96) / liquidity
            } else {
                mul_div(amount, q96(), liquidity)?
            };

            let result = sqrt_p_x96
                .checked_add(quotient)
                .ok_or(SqrtPriceMathError::PriceOverflow)?;

            if result > MAX_UINT160 {
                return Err(SqrtPriceMathError::PriceOverflow);
            }

            Ok(result)
        } else {
            let quotient = if amount <= MAX_UINT160 {
                ref_div_rounding_up(amount << 96, liquidity)?
            } else {
                mul_div_rounding_up(amount, q96(), liquidity)?
            };

            if sqrt_p_x96 <= quotient {
                return Err(SqrtPriceMathError::PriceUnderflow);
            }

            Ok(sqrt_p_x96 - quotient)
        }
    }

    // ─── Deterministic pseudo-random generator ────────────────────────────────

    #[derive(Clone)]
    struct XorShift64 {
        state: u64,
    }

    impl XorShift64 {
        fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.state = x;
            x
        }

        fn u128_as_u256(&mut self) -> U256 {
            U256::from_limbs([self.next_u64(), self.next_u64(), 0, 0])
        }

        fn u160_as_u256_nonzero(&mut self) -> U256 {
            let v = U256::from_limbs([
                self.next_u64(),
                self.next_u64(),
                self.next_u64() & 0xFFFF_FFFF,
                0,
            ]);

            if v.is_zero() { U256::ONE } else { v }
        }

        fn small_amount(&mut self) -> U256 {
            U256::from(self.next_u64() % 1_000_000_000u64)
        }
    }

    // ─── Amount0 delta tests ──────────────────────────────────────────────────

    #[test]
    fn amount0_delta_matches_reference_floor() {
        let cases = [
            (q96(), q96() + U256::from(1u64), U256::from(1u64)),
            (
                q96(),
                q96() + U256::from(1_000_000u64),
                U256::from(1_000_000u64),
            ),
            (
                q96() / U256::from(2u64),
                q96() * U256::from(2u64),
                U256::from(123_456_789u64),
            ),
            (U256::ONE << 80, U256::ONE << 120, U256::ONE << 100),
            (U256::ONE, U256::from(2u64), U256::ONE << 127),
            (
                MAX_UINT160 - U256::from(1_000_000u64),
                MAX_UINT160,
                MAX_UINT128,
            ),
        ];

        for (sqrt_a, sqrt_b, liquidity) in cases {
            let expected = ref_amount0_delta(sqrt_a, sqrt_b, liquidity, false);
            let actual = get_amount0_delta(sqrt_a, sqrt_b, liquidity, false);

            assert_eq!(
                actual, expected,
                "amount0 floor mismatch: sqrt_a={sqrt_a}, sqrt_b={sqrt_b}, liquidity={liquidity}"
            );
        }
    }

    #[test]
    fn amount0_delta_matches_reference_rounding_up() {
        let cases = [
            (q96(), q96() + U256::from(1u64), U256::from(1u64)),
            (
                q96(),
                q96() + U256::from(1_000_000u64),
                U256::from(1_000_000u64),
            ),
            (
                q96() / U256::from(2u64),
                q96() * U256::from(2u64),
                U256::from(123_456_789u64),
            ),
            (U256::ONE << 80, U256::ONE << 120, U256::ONE << 100),
            (U256::ONE, U256::from(2u64), U256::ONE << 127),
            (
                MAX_UINT160 - U256::from(1_000_000u64),
                MAX_UINT160,
                MAX_UINT128,
            ),
        ];

        for (sqrt_a, sqrt_b, liquidity) in cases {
            let expected = ref_amount0_delta(sqrt_a, sqrt_b, liquidity, true);
            let actual = get_amount0_delta(sqrt_a, sqrt_b, liquidity, true);

            assert_eq!(
                actual, expected,
                "amount0 rounding-up mismatch: sqrt_a={sqrt_a}, sqrt_b={sqrt_b}, liquidity={liquidity}"
            );
        }
    }

    #[test]
    fn amount0_delta_is_order_independent() {
        let sqrt_a = U256::ONE << 80;
        let sqrt_b = U256::ONE << 120;
        let liquidity = U256::ONE << 100;

        for round_up in [false, true] {
            let forward = get_amount0_delta(sqrt_a, sqrt_b, liquidity, round_up).unwrap();
            let reverse = get_amount0_delta(sqrt_b, sqrt_a, liquidity, round_up).unwrap();

            assert_eq!(forward, reverse);
        }
    }

    #[test]
    fn amount0_delta_zero_diff_or_zero_liquidity_returns_zero() {
        let sqrt = q96();

        assert_eq!(
            get_amount0_delta(sqrt, sqrt, U256::from(12345u64), false).unwrap(),
            U256::ZERO
        );

        assert_eq!(
            get_amount0_delta(sqrt, sqrt, U256::from(12345u64), true).unwrap(),
            U256::ZERO
        );

        assert_eq!(
            get_amount0_delta(sqrt, sqrt + U256::ONE, U256::ZERO, false).unwrap(),
            U256::ZERO
        );

        assert_eq!(
            get_amount0_delta(sqrt, sqrt + U256::ONE, U256::ZERO, true).unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn amount0_delta_random_matches_reference() {
        let mut rng = XorShift64::new(0xA0D0_0001_1234_5678);

        for _ in 0..512 {
            let a = rng.u160_as_u256_nonzero();
            let b = rng.u160_as_u256_nonzero();
            let liquidity = rng.u128_as_u256();

            for round_up in [false, true] {
                let expected = ref_amount0_delta(a, b, liquidity, round_up);
                let actual = get_amount0_delta(a, b, liquidity, round_up);

                assert_eq!(
                    actual, expected,
                    "random amount0 mismatch: a={a}, b={b}, liquidity={liquidity}, round_up={round_up}"
                );
            }
        }
    }

    // ─── Amount1 delta tests ──────────────────────────────────────────────────

    #[test]
    fn amount1_delta_matches_reference_floor_and_rounding_up() {
        let cases = [
            (q96(), q96() + U256::from(1u64), U256::from(1u64)),
            (
                q96(),
                q96() + U256::from(1_000_000u64),
                U256::from(1_000_000u64),
            ),
            (U256::ONE << 80, U256::ONE << 120, U256::ONE << 100),
            (
                MAX_UINT160 - U256::from(1_000_000u64),
                MAX_UINT160,
                MAX_UINT128,
            ),
        ];

        for (sqrt_a, sqrt_b, liquidity) in cases {
            for round_up in [false, true] {
                let expected = ref_amount1_delta(sqrt_a, sqrt_b, liquidity, round_up);
                let actual = get_amount1_delta(sqrt_a, sqrt_b, liquidity, round_up);

                assert_eq!(
                    actual, expected,
                    "amount1 mismatch: sqrt_a={sqrt_a}, sqrt_b={sqrt_b}, liquidity={liquidity}, round_up={round_up}"
                );
            }
        }
    }

    #[test]
    fn amount1_delta_is_order_independent() {
        let sqrt_a = U256::ONE << 80;
        let sqrt_b = U256::ONE << 120;
        let liquidity = U256::ONE << 100;

        for round_up in [false, true] {
            let forward = get_amount1_delta(sqrt_a, sqrt_b, liquidity, round_up).unwrap();
            let reverse = get_amount1_delta(sqrt_b, sqrt_a, liquidity, round_up).unwrap();

            assert_eq!(forward, reverse);
        }
    }

    #[test]
    fn amount1_delta_zero_diff_or_zero_liquidity_returns_zero() {
        let sqrt = q96();

        assert_eq!(
            get_amount1_delta(sqrt, sqrt, U256::from(12345u64), false).unwrap(),
            U256::ZERO
        );

        assert_eq!(
            get_amount1_delta(sqrt, sqrt, U256::from(12345u64), true).unwrap(),
            U256::ZERO
        );

        assert_eq!(
            get_amount1_delta(sqrt, sqrt + U256::ONE, U256::ZERO, false).unwrap(),
            U256::ZERO
        );

        assert_eq!(
            get_amount1_delta(sqrt, sqrt + U256::ONE, U256::ZERO, true).unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn amount1_delta_random_matches_reference() {
        let mut rng = XorShift64::new(0xA1D1_0002_8765_4321);

        for _ in 0..512 {
            let a = rng.u160_as_u256_nonzero();
            let b = rng.u160_as_u256_nonzero();
            let liquidity = rng.u128_as_u256();

            for round_up in [false, true] {
                let expected = ref_amount1_delta(a, b, liquidity, round_up);
                let actual = get_amount1_delta(a, b, liquidity, round_up);

                assert_eq!(
                    actual, expected,
                    "random amount1 mismatch: a={a}, b={b}, liquidity={liquidity}, round_up={round_up}"
                );
            }
        }
    }

    // ─── Next sqrt price: amount0 tests ───────────────────────────────────────

    #[test]
    fn next_sqrt_price_amount0_add_matches_reference() {
        let cases = [
            (q96(), U256::from(1_000_000u64), U256::from(100u64)),
            (U256::ONE << 120, U256::ONE << 100, U256::ONE << 40),
            (q96(), MAX_UINT128, U256::ONE << 200),
            (
                MAX_UINT160 - U256::from(1_000_000u64),
                MAX_UINT128,
                U256::from(1u64),
            ),
        ];

        for (sqrt_p, liquidity, amount) in cases {
            let expected =
                ref_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount, true);
            let actual =
                get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount, true);

            assert_eq!(
                actual, expected,
                "amount0 add mismatch: sqrt_p={sqrt_p}, liquidity={liquidity}, amount={amount}"
            );
        }
    }

    #[test]
    fn next_sqrt_price_amount0_remove_matches_reference() {
        let cases = [
            (q96(), U256::from(1_000_000u64), U256::from(1u64)),
            (U256::ONE << 120, U256::ONE << 100, U256::ONE << 10),
            (
                MAX_UINT160 - U256::from(1_000_000u64),
                MAX_UINT128,
                U256::from(1u64),
            ),
        ];

        for (sqrt_p, liquidity, amount) in cases {
            let expected =
                ref_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount, false);
            let actual =
                get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount, false);

            assert_eq!(
                actual, expected,
                "amount0 remove mismatch: sqrt_p={sqrt_p}, liquidity={liquidity}, amount={amount}"
            );
        }
    }

    #[test]
    fn next_sqrt_price_amount0_remove_insufficient_reserves() {
        let sqrt_p = q96();
        let liquidity = U256::from(1_000_000u64);

        // product = amount * sqrtP equals liquidity * Q96, so denominator would
        // be zero. The function must reject it.
        let amount = liquidity;

        assert_eq!(
            get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount, false),
            Err(SqrtPriceMathError::InsufficientToken0Reserves)
        );
    }

    #[test]
    fn next_sqrt_price_amount0_amount_zero_returns_current_price() {
        let sqrt_p = q96();
        let liquidity = U256::from(1_000_000u64);

        assert_eq!(
            get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, U256::ZERO, true)
                .unwrap(),
            sqrt_p
        );

        assert_eq!(
            get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, U256::ZERO, false)
                .unwrap(),
            sqrt_p
        );
    }

    #[test]
    fn next_sqrt_price_amount0_random_matches_reference() {
        let mut rng = XorShift64::new(0xA0A0_3333_9999_1111);

        for _ in 0..256 {
            let sqrt_p = rng.u160_as_u256_nonzero();
            let liquidity = {
                let v = rng.u128_as_u256();
                if v.is_zero() { U256::ONE } else { v }
            };

            // Keep most random cases within realistic swap sizes so both
            // success and failure paths are exercised without making the test
            // suite too slow.
            let amount = rng.small_amount();

            for add in [true, false] {
                let expected =
                    ref_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount, add);
                let actual =
                    get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount, add);

                assert_eq!(
                    actual, expected,
                    "random amount0 next price mismatch: sqrt_p={sqrt_p}, liquidity={liquidity}, amount={amount}, add={add}"
                );
            }
        }
    }

    // ─── Next sqrt price: amount1 tests ───────────────────────────────────────

    #[test]
    fn next_sqrt_price_amount1_add_matches_reference() {
        let cases = [
            (q96(), U256::from(1_000_000u64), U256::from(100u64)),
            (U256::ONE << 120, U256::ONE << 100, U256::ONE << 40),
            (q96(), MAX_UINT128, MAX_UINT160 + U256::ONE),
        ];

        for (sqrt_p, liquidity, amount) in cases {
            let expected =
                ref_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount, true);
            let actual =
                get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount, true);

            assert_eq!(
                actual, expected,
                "amount1 add mismatch: sqrt_p={sqrt_p}, liquidity={liquidity}, amount={amount}"
            );
        }
    }

    #[test]
    fn next_sqrt_price_amount1_remove_matches_reference() {
        let cases = [
            (q96(), U256::from(1_000_000u64), U256::from(1u64)),
            (U256::ONE << 120, U256::ONE << 100, U256::ONE << 10),
            (
                MAX_UINT160 - U256::from(1_000_000u64),
                MAX_UINT128,
                U256::from(1u64),
            ),
        ];

        for (sqrt_p, liquidity, amount) in cases {
            let expected =
                ref_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount, false);
            let actual =
                get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount, false);

            assert_eq!(
                actual, expected,
                "amount1 remove mismatch: sqrt_p={sqrt_p}, liquidity={liquidity}, amount={amount}"
            );
        }
    }

    #[test]
    fn next_sqrt_price_amount1_remove_underflow() {
        let sqrt_p = q96();
        let liquidity = U256::from(1u64);
        let amount = U256::from(1u64);

        assert_eq!(
            get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount, false),
            Err(SqrtPriceMathError::PriceUnderflow)
        );
    }

    #[test]
    fn next_sqrt_price_amount1_amount_zero_returns_current_price() {
        let sqrt_p = q96();
        let liquidity = U256::from(1_000_000u64);

        assert_eq!(
            get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, U256::ZERO, true)
                .unwrap(),
            sqrt_p
        );

        assert_eq!(
            get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, U256::ZERO, false)
                .unwrap(),
            sqrt_p
        );
    }

    #[test]
    fn next_sqrt_price_amount1_random_matches_reference() {
        let mut rng = XorShift64::new(0xB1B1_4444_2222_7777);

        for _ in 0..256 {
            let sqrt_p = rng.u160_as_u256_nonzero();
            let liquidity = {
                let v = rng.u128_as_u256();
                if v.is_zero() { U256::ONE } else { v }
            };

            let amount = rng.small_amount();

            for add in [true, false] {
                let expected =
                    ref_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount, add);
                let actual =
                    get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount, add);

                assert_eq!(
                    actual, expected,
                    "random amount1 next price mismatch: sqrt_p={sqrt_p}, liquidity={liquidity}, amount={amount}, add={add}"
                );
            }
        }
    }

    // ─── High-level input/output dispatch tests ───────────────────────────────

    #[test]
    fn next_sqrt_price_from_input_dispatches_correctly() {
        let sqrt_p = q96();
        let liquidity = U256::from(1_000_000u64);
        let amount_in = U256::from(100u64);

        assert_eq!(
            get_next_sqrt_price_from_input(sqrt_p, liquidity, amount_in, true),
            get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount_in, true)
        );

        assert_eq!(
            get_next_sqrt_price_from_input(sqrt_p, liquidity, amount_in, false),
            get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount_in, true)
        );
    }

    #[test]
    fn next_sqrt_price_from_output_dispatches_correctly() {
        let sqrt_p = q96();
        let liquidity = U256::from(1_000_000u64);
        let amount_out = U256::from(1u64);

        assert_eq!(
            get_next_sqrt_price_from_output(sqrt_p, liquidity, amount_out, true),
            get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount_out, false)
        );

        assert_eq!(
            get_next_sqrt_price_from_output(sqrt_p, liquidity, amount_out, false),
            get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount_out, false)
        );
    }

    // ─── Domain guard tests ───────────────────────────────────────────────────

    #[test]
    fn zero_price_errors() {
        assert_eq!(
            get_amount0_delta(U256::ZERO, q96(), U256::ONE, false),
            Err(SqrtPriceMathError::ZeroPrice)
        );

        assert_eq!(
            get_amount1_delta(U256::ZERO, q96(), U256::ONE, false),
            Err(SqrtPriceMathError::ZeroPrice)
        );

        assert_eq!(
            get_next_sqrt_price_from_amount0_rounding_up(U256::ZERO, U256::ONE, U256::ONE, true),
            Err(SqrtPriceMathError::ZeroPrice)
        );

        assert_eq!(
            get_next_sqrt_price_from_amount1_rounding_down(U256::ZERO, U256::ONE, U256::ONE, true),
            Err(SqrtPriceMathError::ZeroPrice)
        );
    }

    #[test]
    fn zero_liquidity_errors_for_next_price_but_not_delta() {
        assert_eq!(
            get_next_sqrt_price_from_amount0_rounding_up(q96(), U256::ZERO, U256::ONE, true),
            Err(SqrtPriceMathError::ZeroLiquidity)
        );

        assert_eq!(
            get_next_sqrt_price_from_amount1_rounding_down(q96(), U256::ZERO, U256::ONE, true),
            Err(SqrtPriceMathError::ZeroLiquidity)
        );

        assert_eq!(
            get_amount0_delta(q96(), q96() + U256::ONE, U256::ZERO, false).unwrap(),
            U256::ZERO
        );

        assert_eq!(
            get_amount1_delta(q96(), q96() + U256::ONE, U256::ZERO, false).unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn rejects_sqrt_price_above_uint160() {
        let invalid_sqrt = MAX_UINT160 + U256::ONE;

        assert_eq!(
            get_next_sqrt_price_from_amount0_rounding_up(invalid_sqrt, U256::ONE, U256::ONE, true),
            Err(SqrtPriceMathError::PriceOverflow)
        );

        assert_eq!(
            get_next_sqrt_price_from_amount1_rounding_down(
                invalid_sqrt,
                U256::ONE,
                U256::ONE,
                true
            ),
            Err(SqrtPriceMathError::PriceOverflow)
        );

        assert_eq!(
            get_amount0_delta(q96(), invalid_sqrt, U256::ONE, false),
            Err(SqrtPriceMathError::PriceOverflow)
        );

        assert_eq!(
            get_amount1_delta(q96(), invalid_sqrt, U256::ONE, false),
            Err(SqrtPriceMathError::PriceOverflow)
        );
    }

    #[test]
    fn rejects_liquidity_above_uint128() {
        let invalid_liquidity = MAX_UINT128 + U256::ONE;

        assert_eq!(
            get_next_sqrt_price_from_amount0_rounding_up(q96(), invalid_liquidity, U256::ONE, true),
            Err(SqrtPriceMathError::PriceOverflow)
        );

        assert_eq!(
            get_next_sqrt_price_from_amount1_rounding_down(
                q96(),
                invalid_liquidity,
                U256::ONE,
                true
            ),
            Err(SqrtPriceMathError::PriceOverflow)
        );

        assert_eq!(
            get_amount0_delta(q96(), q96() + U256::ONE, invalid_liquidity, false),
            Err(SqrtPriceMathError::PriceOverflow)
        );

        assert_eq!(
            get_amount1_delta(q96(), q96() + U256::ONE, invalid_liquidity, false),
            Err(SqrtPriceMathError::PriceOverflow)
        );
    }

    // ─── Public div helper tests ──────────────────────────────────────────────

    #[test]
    fn div_rounding_up_works() {
        assert_eq!(
            div_rounding_up(U256::from(10u64), U256::from(5u64)).unwrap(),
            U256::from(2u64)
        );

        assert_eq!(
            div_rounding_up(U256::from(10u64), U256::from(3u64)).unwrap(),
            U256::from(4u64)
        );

        assert_eq!(
            div_rounding_up(U256::ZERO, U256::from(3u64)).unwrap(),
            U256::ZERO
        );

        assert_eq!(
            div_rounding_up(U256::ONE, U256::ZERO),
            Err(SqrtPriceMathError::ZeroDenominator)
        );
    }
}
