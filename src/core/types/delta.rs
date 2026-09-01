use ruint::aliases::U256;

use crate::{
    Error,
    core::{
        math::sqrt_price::{get_amount0_delta, get_amount1_delta},
        types::{sqrt_price::SqrtPriceX96, tick::TickIndex},
    },
    v4::Liquidity,
};

/// Swap amount adjustments returned by a `before_swap` hook.
///
/// V4 splits the hook's pre-swap adjustment into the specified side of the
/// swap and the unspecified side. The adapter later maps those two signed
/// values onto token0/token1 according to swap direction and exact
/// input/output mode.
pub struct BeforeSwapDelta {
    /// Adjustment to `SwapParams::amount_specified` before swap execution.
    ///
    /// Negative values increase exact-input size or reduce exact-output target;
    /// positive values reduce exact-input size or increase exact-output target.
    pub specified_delta: i128,

    /// Hook-owned delta for the opposite side of the swap.
    ///
    /// This is accumulated with the hook's `after_swap` return value before the
    /// final hook `BalanceDelta` is derived.
    pub unspecified_delta: i128,
}

impl BeforeSwapDelta {
    /// No hook adjustment.
    pub const ZERO: Self = Self {
        specified_delta: 0,
        unspecified_delta: 0,
    };
}

/// Signed token delta using Uniswap V4's **caller / PoolManager-perspective**
/// `BalanceDelta` convention, matching the convention used by the current
/// swap simulator.
///
/// # Sign convention
///
/// | Value | Caller / PoolManager-perspective meaning          |
/// |-------|----------------------------------------------------|
/// | `< 0` | Caller owes/sends this token to the PoolManager.   |
/// | `> 0` | Caller receives/is owed this token from the pool.  |
///
/// Example — `zero_for_one` exact-input swap:
/// - `amount0 < 0` — caller paid/owes token0.
/// - `amount1 > 0` — caller receives token1.
///
/// Use [`BalanceDelta::amount_in`] / [`BalanceDelta::amount_out`] for
/// direction-aware unsigned accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceDelta {
    /// Token0 delta.
    ///
    /// Caller perspective:
    /// - negative = caller pays/owes token0
    /// - positive = caller receives token0
    pub amount0: i128,

    /// Token1 delta.
    ///
    /// Caller perspective:
    /// - negative = caller pays/owes token1
    /// - positive = caller receives token1
    pub amount1: i128,
}

impl BalanceDelta {
    pub const DEFAULT: Self = BalanceDelta {
        amount0: 0,
        amount1: 0,
    };

    #[inline(always)]
    pub(crate) fn new(amount0: i128, amount1: i128) -> Self {
        Self { amount0, amount1 }
    }

    pub(crate) fn from_u256(amount0: U256, amount1: U256) -> Result<Self, crate::Error> {
        Ok(Self {
            amount0: u256_to_i128_positive(amount0)?,
            amount1: u256_to_i128_positive(amount1)?,
        })
    }

    /// Builds the principal token delta for a liquidity modification.
    ///
    /// The returned delta follows V4's caller / PoolManager convention:
    /// adding liquidity is negative because the caller deposits principal,
    /// while removing liquidity is positive because the caller receives it.
    pub(crate) fn from_liquidity_principal(
        tick_current: TickIndex,
        tick_lower: TickIndex,
        tick_upper: TickIndex,
        sqrt_price_current_x96: SqrtPriceX96,
        sqrt_price_lower_x96: SqrtPriceX96,
        sqrt_price_upper_x96: SqrtPriceX96,
        liquidity_delta: i128,
    ) -> Result<Self, Error> {
        let liquidity_delta_positive = liquidity_delta >= 0;
        let liquidity_delta = Liquidity::new(liquidity_delta.unsigned_abs());

        if tick_current < tick_lower {
            return Ok(Self {
                amount0: amount0_delta_for_liquidity_change(
                    sqrt_price_lower_x96,
                    sqrt_price_upper_x96,
                    liquidity_delta,
                    liquidity_delta_positive,
                )?,
                amount1: 0,
            });
        }

        if tick_current < tick_upper {
            return Ok(Self {
                amount0: amount0_delta_for_liquidity_change(
                    sqrt_price_current_x96,
                    sqrt_price_upper_x96,
                    liquidity_delta,
                    liquidity_delta_positive,
                )?,
                amount1: amount1_delta_for_liquidity_change(
                    sqrt_price_lower_x96,
                    sqrt_price_current_x96,
                    liquidity_delta,
                    liquidity_delta_positive,
                )?,
            });
        }

        Ok(Self {
            amount0: 0,
            amount1: amount1_delta_for_liquidity_change(
                sqrt_price_lower_x96,
                sqrt_price_upper_x96,
                liquidity_delta,
                liquidity_delta_positive,
            )?,
        })
    }

    /// Direction-aware unsigned output amount.
    ///
    /// Caller / PoolManager perspective:
    /// output token is positive because the caller receives it.
    #[inline]
    #[must_use]
    pub fn amount_out(&self, zero_for_one: bool) -> u128 {
        let raw = if zero_for_one {
            self.amount1 // caller receives token1
        } else {
            self.amount0 // caller receives token0
        };

        if raw > 0 { raw as u128 } else { 0 }
    }

    /// Direction-aware unsigned input amount.
    ///
    /// Caller / PoolManager perspective:
    /// input token is negative because the caller pays/owes it.
    #[inline]
    #[must_use]
    pub fn amount_in(&self, zero_for_one: bool) -> u128 {
        let raw = if zero_for_one {
            self.amount0 // caller pays/owes token0
        } else {
            self.amount1 // caller pays/owes token1
        };

        if raw < 0 { raw.unsigned_abs() } else { 0 }
    }

    /// Subtracts another signed delta, returning `I128Overflow` on overflow.
    ///
    /// This is used when hook-owned deltas are removed from a pool-produced
    /// operation delta to derive the caller-settled remainder.
    #[inline]
    pub fn checked_sub(&self, other: &Self) -> Result<Self, crate::Error> {
        let amount0 = self
            .amount0
            .checked_sub(other.amount0)
            .ok_or(crate::Error::I128Overflow)?;
        let amount1 = self
            .amount1
            .checked_sub(other.amount1)
            .ok_or(crate::Error::I128Overflow)?;
        Ok(Self { amount0, amount1 })
    }

    /// Adds another signed delta, returning `I128Overflow` on overflow.
    #[inline]
    pub fn checked_add(&self, other: &Self) -> Result<Self, crate::Error> {
        let amount0 = self
            .amount0
            .checked_add(other.amount0)
            .ok_or(crate::Error::I128Overflow)?;
        let amount1 = self
            .amount1
            .checked_add(other.amount1)
            .ok_or(crate::Error::I128Overflow)?;
        Ok(Self { amount0, amount1 })
    }
}

fn amount0_delta_for_liquidity_change(
    sqrt_price_a_x96: SqrtPriceX96,
    sqrt_price_b_x96: SqrtPriceX96,
    liquidity_delta: Liquidity,
    liquidity_delta_positive: bool,
) -> Result<i128, Error> {
    let amount = get_amount0_delta(
        sqrt_price_a_x96,
        sqrt_price_b_x96,
        liquidity_delta,
        liquidity_delta_positive,
    )
    .map(|(amount, _)| amount)
    .map_err(|_| Error::AmountOverflow)?;
    liquidity_amount_to_i128(amount, liquidity_delta_positive)
}

fn amount1_delta_for_liquidity_change(
    sqrt_price_a_x96: SqrtPriceX96,
    sqrt_price_b_x96: SqrtPriceX96,
    liquidity_delta: Liquidity,
    liquidity_delta_positive: bool,
) -> Result<i128, Error> {
    let amount = get_amount1_delta(
        sqrt_price_a_x96,
        sqrt_price_b_x96,
        liquidity_delta,
        liquidity_delta_positive,
    )
    .map(|(amount, _)| amount)
    .map_err(|_| Error::AmountOverflow)?;
    liquidity_amount_to_i128(amount, liquidity_delta_positive)
}

#[inline(always)]
fn liquidity_amount_to_i128(amount: U256, liquidity_delta_positive: bool) -> Result<i128, Error> {
    if liquidity_delta_positive {
        u256_to_i128_negative(amount)
    } else {
        u256_to_i128_positive(amount)
    }
}

#[inline(always)]
fn u256_to_i128_positive(amount: U256) -> Result<i128, Error> {
    let [lo, hi, upper0, upper1] = amount.into_limbs();

    if (upper0 | upper1) != 0 || (hi >> 63) != 0 {
        return Err(Error::AmountOverflow);
    }

    Ok((((hi as u128) << 64) | lo as u128) as i128)
}

#[inline(always)]
fn u256_to_i128_negative(amount: U256) -> Result<i128, Error> {
    let [lo, hi, upper0, upper1] = amount.into_limbs();

    const LIMIT_HI: u64 = 1 << 63;

    if (upper0 | upper1) != 0 || hi > LIMIT_HI || (hi == LIMIT_HI && lo != 0) {
        return Err(Error::AmountOverflow);
    }

    let magnitude = ((hi as u128) << 64) | lo as u128;

    Ok((magnitude as i128).wrapping_neg())
}

#[cfg(test)]
mod tests {

    use crate::core::{
        math::tick::get_sqrt_price_at_tick,
        types::{sqrt_price::SqrtPriceX96, tick::TickIndex},
    };

    use super::BalanceDelta;

    fn tick(index: i32) -> TickIndex {
        TickIndex::new(index).expect("tick index out of range")
    }

    fn sqrt_at(index: i32) -> SqrtPriceX96 {
        get_sqrt_price_at_tick(tick(index))
    }

    #[test]
    fn zero_for_one_exact_input_uses_caller_perspective() {
        let delta = BalanceDelta {
            amount0: -1_000,
            amount1: 990,
        };

        assert_eq!(delta.amount_in(true), 1_000);
        assert_eq!(delta.amount_out(true), 990);
    }

    #[test]
    fn one_for_zero_exact_input_uses_caller_perspective() {
        let delta = BalanceDelta {
            amount0: 990,
            amount1: -1_000,
        };

        assert_eq!(delta.amount_in(false), 1_000);
        assert_eq!(delta.amount_out(false), 990);
    }

    #[test]
    fn wrong_sign_returns_zero_instead_of_wrong_amount() {
        let delta = BalanceDelta {
            amount0: 1_000,
            amount1: -990,
        };

        assert_eq!(delta.amount_in(true), 0);
        assert_eq!(delta.amount_out(true), 0);
    }

    #[test]
    fn from_liquidity_principal_below_range_uses_only_token0() {
        let delta = BalanceDelta::from_liquidity_principal(
            tick(-240),
            tick(-120),
            tick(120),
            sqrt_at(-240),
            sqrt_at(-120),
            sqrt_at(120),
            1_000_000,
        )
        .unwrap();

        assert!(delta.amount0 < 0);
        assert_eq!(delta.amount1, 0);
    }

    #[test]
    fn from_liquidity_principal_in_range_uses_both_tokens() {
        let delta = BalanceDelta::from_liquidity_principal(
            tick(0),
            tick(-120),
            tick(120),
            sqrt_at(0),
            sqrt_at(-120),
            sqrt_at(120),
            1_000_000,
        )
        .unwrap();

        assert!(delta.amount0 < 0);
        assert!(delta.amount1 < 0);
    }

    #[test]
    fn from_liquidity_principal_above_range_uses_only_token1() {
        let delta = BalanceDelta::from_liquidity_principal(
            tick(240),
            tick(-120),
            tick(120),
            sqrt_at(240),
            sqrt_at(-120),
            sqrt_at(120),
            1_000_000,
        )
        .unwrap();

        assert_eq!(delta.amount0, 0);
        assert!(delta.amount1 < 0);
    }

    #[test]
    fn from_liquidity_principal_remove_liquidity_returns_positive_delta() {
        let delta = BalanceDelta::from_liquidity_principal(
            tick(0),
            tick(-120),
            tick(120),
            sqrt_at(0),
            sqrt_at(-120),
            sqrt_at(120),
            -1_000_000,
        )
        .unwrap();

        assert!(delta.amount0 > 0);
        assert!(delta.amount1 > 0);
    }
}
