//! # SwapMath — Optimized single-tick swap step computation
//!
//! Production-focused Rust port of Uniswap V3/V4 swap-step math.
//!
//! Main optimizations:
//! - No tracing in the hot arithmetic path.
//! - No generic FullMath for fee arithmetic.
//! - Fee math uses exact U256 × u32 / u32 long division.
//! - Fee complement is computed once and passed down.
//! - Exact-input partial-fill accounting preserves V4-style semantics:
//!   if target is not reached, amount_in is the full fee-adjusted input.

use ruint::aliases::U256;

use crate::v3::amount::SignedAmount;

use super::full_math::MathError;
use super::sqrt_price_math::{
    get_amount0_delta, get_amount1_delta, get_next_sqrt_price_from_input,
    get_next_sqrt_price_from_output, SqrtPriceMathError,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum swap fee: 100%, expressed in hundredths of a bip.
pub const MAX_SWAP_FEE: u32 = 1_000_000;

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapMathError {
    FeeTooLarge,
    MaxFeeExactOut,
    Math(MathError),
    SqrtPrice(SqrtPriceMathError),
}

impl core::fmt::Display for SwapMathError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SwapMathError::FeeTooLarge => f.write_str("swap_math: fee_pips exceeds MAX_SWAP_FEE"),
            SwapMathError::MaxFeeExactOut => {
                f.write_str("swap_math: exact-out swap with 100% fee is undefined")
            }
            SwapMathError::Math(e) => write!(f, "swap_math: full_math error: {e}"),
            SwapMathError::SqrtPrice(e) => write!(f, "swap_math: sqrt_price_math error: {e:?}"),
        }
    }
}

impl From<MathError> for SwapMathError {
    #[inline(always)]
    fn from(e: MathError) -> Self {
        SwapMathError::Math(e)
    }
}

impl From<SqrtPriceMathError> for SwapMathError {
    #[inline(always)]
    fn from(e: SqrtPriceMathError) -> Self {
        SwapMathError::SqrtPrice(e)
    }
}

// ── Cold error constructors ───────────────────────────────────────────────────

#[cold]
#[inline(never)]
fn err_fee_too_large() -> SwapMathError {
    SwapMathError::FeeTooLarge
}

#[cold]
#[inline(never)]
fn err_max_fee_exact_out() -> SwapMathError {
    SwapMathError::MaxFeeExactOut
}

// ── Output type ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapStep {
    pub sqrt_ratio_next_x96: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

// ── Fee arithmetic ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct FeeParams {
    pips: u32,
    complement: u32,
}

impl FeeParams {
    #[inline(always)]
    fn new(fee_pips: u32) -> Result<Self, SwapMathError> {
        if fee_pips > MAX_SWAP_FEE {
            return Err(err_fee_too_large());
        }

        Ok(Self {
            pips: fee_pips,
            complement: MAX_SWAP_FEE - fee_pips,
        })
    }

    #[inline(always)]
    fn is_zero(self) -> bool {
        self.pips == 0
    }

    #[inline(always)]
    fn is_max(self) -> bool {
        self.pips == MAX_SWAP_FEE
    }
}

/// Exact helper for:
///
///     floor(a * mul / div)
///
/// where `mul` and `div` are small u32 values.
///
/// This avoids generic FullMath for fee calculations. The product is represented
/// as five 64-bit limbs because U256 * u32 can require up to 276 bits.
#[inline(always)]
fn mul_div_u32_floor(a: U256, mul: u32, div: u32) -> Result<U256, MathError> {
    let (q, _) = mul_div_u32_div_rem(a, mul, div)?;
    Ok(q)
}

/// Exact helper for:
///
///     ceil(a * mul / div)
///
/// where `mul` and `div` are small u32 values.
#[inline(always)]
fn mul_div_u32_ceil(a: U256, mul: u32, div: u32) -> Result<U256, MathError> {
    let (q, r) = mul_div_u32_div_rem(a, mul, div)?;

    if r == 0 {
        Ok(q)
    } else {
        q.checked_add(U256::ONE).ok_or(MathError::Overflow)
    }
}

/// Exact U256 × u32 / u32 long division.
///
/// Returns `(quotient, remainder)`.
#[inline(always)]
fn mul_div_u32_div_rem(a: U256, mul: u32, div: u32) -> Result<(U256, u32), MathError> {
    if div == 0 {
        return Err(MathError::ZeroDenominator);
    }

    if a.is_zero() || mul == 0 {
        return Ok((U256::ZERO, 0));
    }

    if mul == div {
        return Ok((a, 0));
    }

    let mul = mul as u128;
    let div = div as u128;

    let [a0, a1, a2, a3] = a.into_limbs();

    // Multiply U256 by u32 into 5 little-endian u64 limbs.
    let mut product = [0u64; 5];
    let mut carry = 0u128;

    let t0 = (a0 as u128) * mul + carry;
    product[0] = t0 as u64;
    carry = t0 >> 64;

    let t1 = (a1 as u128) * mul + carry;
    product[1] = t1 as u64;
    carry = t1 >> 64;

    let t2 = (a2 as u128) * mul + carry;
    product[2] = t2 as u64;
    carry = t2 >> 64;

    let t3 = (a3 as u128) * mul + carry;
    product[3] = t3 as u64;
    carry = t3 >> 64;

    product[4] = carry as u64;

    // Divide the 5-limb product by u32 using base 2^64 long division.
    let mut q = [0u64; 5];
    let mut rem = 0u128;

    let cur4 = (rem << 64) | product[4] as u128;
    q[4] = (cur4 / div) as u64;
    rem = cur4 % div;

    let cur3 = (rem << 64) | product[3] as u128;
    q[3] = (cur3 / div) as u64;
    rem = cur3 % div;

    let cur2 = (rem << 64) | product[2] as u128;
    q[2] = (cur2 / div) as u64;
    rem = cur2 % div;

    let cur1 = (rem << 64) | product[1] as u128;
    q[1] = (cur1 / div) as u64;
    rem = cur1 % div;

    let cur0 = (rem << 64) | product[0] as u128;
    q[0] = (cur0 / div) as u64;
    rem = cur0 % div;

    if q[4] != 0 {
        return Err(MathError::Overflow);
    }

    Ok((U256::from_limbs([q[0], q[1], q[2], q[3]]), rem as u32))
}

#[inline(always)]
fn amount_less_fee_exact_in(amount_remaining: U256, fee: FeeParams) -> Result<U256, MathError> {
    if fee.is_zero() {
        return Ok(amount_remaining);
    }

    if fee.is_max() {
        return Ok(U256::ZERO);
    }

    mul_div_u32_floor(amount_remaining, fee.complement, MAX_SWAP_FEE)
}

#[inline(always)]
fn fee_on_exact_input(amount_in: U256, fee: FeeParams) -> Result<U256, MathError> {
    if amount_in.is_zero() || fee.is_zero() {
        return Ok(U256::ZERO);
    }

    if fee.is_max() {
        return Ok(amount_in);
    }

    // fee = ceil(amount_in * fee_pips / (MAX_SWAP_FEE - fee_pips))
    mul_div_u32_ceil(amount_in, fee.pips, fee.complement)
}

// ── Price-target helper ───────────────────────────────────────────────────────

#[inline(always)]
pub fn get_sqrt_price_target(
    zero_for_one: bool,
    sqrt_price_next_x96: U256,
    sqrt_price_limit_x96: U256,
) -> U256 {
    if zero_for_one {
        sqrt_price_next_x96.max(sqrt_price_limit_x96)
    } else {
        sqrt_price_next_x96.min(sqrt_price_limit_x96)
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

#[inline]
#[must_use = "discarding a swap step result silently drops errors"]
pub fn compute_swap_step(
    sqrt_ratio_current_x96: U256,
    sqrt_ratio_target_x96: U256,
    liquidity: u128,
    amount: SignedAmount,
    fee_pips: u32,
) -> Result<SwapStep, SwapMathError> {
    let fee = FeeParams::new(fee_pips)?;

    // Keep original behavior: fee is validated before zero-amount no-op.
    if amount.is_zero() {
        return Ok(SwapStep {
            sqrt_ratio_next_x96: sqrt_ratio_current_x96,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            fee_amount: U256::ZERO,
        });
    }

    let exact_in = amount.is_negative();

    if !exact_in && fee.is_max() {
        return Err(err_max_fee_exact_out());
    }

    let zero_for_one = sqrt_ratio_current_x96 >= sqrt_ratio_target_x96;
    let liquidity = U256::from(liquidity);
    let amount_remaining = amount.abs();

    if exact_in {
        compute_exact_in(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            amount_remaining,
            fee,
            zero_for_one,
        )
    } else {
        compute_exact_out(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            amount_remaining,
            fee,
            zero_for_one,
        )
    }
}

#[inline]
#[must_use]
pub fn compute_swap_step_unwrap(
    sqrt_ratio_current_x96: U256,
    sqrt_ratio_target_x96: U256,
    liquidity: u128,
    amount: SignedAmount,
    fee_pips: u32,
) -> SwapStep {
    compute_swap_step(
        sqrt_ratio_current_x96,
        sqrt_ratio_target_x96,
        liquidity,
        amount,
        fee_pips,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

// ── Internal exact-input path ─────────────────────────────────────────────────

#[inline]
fn compute_exact_in(
    sqrt_ratio_current_x96: U256,
    sqrt_ratio_target_x96: U256,
    liquidity: U256,
    amount_remaining: U256,
    fee: FeeParams,
    zero_for_one: bool,
) -> Result<SwapStep, SwapMathError> {
    let amount_remaining_less_fee = amount_less_fee_exact_in(amount_remaining, fee)?;

    let max_amount_in = if zero_for_one {
        get_amount0_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity,
            true,
        )?
    } else {
        get_amount1_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            true,
        )?
    };

    let reached_target = amount_remaining_less_fee >= max_amount_in;

    let (sqrt_ratio_next_x96, amount_in) = if reached_target {
        (sqrt_ratio_target_x96, max_amount_in)
    } else {
        // V4-compatible accounting:
        // In a partial exact-input fill, consume the full fee-adjusted input.
        // Do not recompute amount_in from the rounded next price.
        let next = get_next_sqrt_price_from_input(
            sqrt_ratio_current_x96,
            liquidity,
            amount_remaining_less_fee,
            zero_for_one,
        )?;

        (next, amount_remaining_less_fee)
    };

    let amount_out = if zero_for_one {
        get_amount1_delta(
            sqrt_ratio_next_x96,
            sqrt_ratio_current_x96,
            liquidity,
            false,
        )?
    } else {
        get_amount0_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_next_x96,
            liquidity,
            false,
        )?
    };

    let fee_amount = if reached_target {
        fee_on_exact_input(amount_in, fee)?
    } else {
        // Safety: amount_in == amount_remaining_less_fee <= amount_remaining.
        amount_remaining - amount_in
    };

    Ok(SwapStep {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

// ── Internal exact-output path ────────────────────────────────────────────────

#[inline]
fn compute_exact_out(
    sqrt_ratio_current_x96: U256,
    sqrt_ratio_target_x96: U256,
    liquidity: U256,
    amount_remaining: U256,
    fee: FeeParams,
    zero_for_one: bool,
) -> Result<SwapStep, SwapMathError> {
    debug_assert!(!fee.is_max());

    let max_amount_out = if zero_for_one {
        get_amount1_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity,
            false,
        )?
    } else {
        get_amount0_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            false,
        )?
    };

    let (sqrt_ratio_next_x96, amount_out) = if amount_remaining >= max_amount_out {
        (sqrt_ratio_target_x96, max_amount_out)
    } else {
        let next = get_next_sqrt_price_from_output(
            sqrt_ratio_current_x96,
            liquidity,
            amount_remaining,
            zero_for_one,
        )?;

        (next, amount_remaining)
    };

    let amount_in = if zero_for_one {
        get_amount0_delta(sqrt_ratio_next_x96, sqrt_ratio_current_x96, liquidity, true)?
    } else {
        get_amount1_delta(sqrt_ratio_current_x96, sqrt_ratio_next_x96, liquidity, true)?
    };

    let fee_amount = fee_on_exact_input(amount_in, fee)?;

    Ok(SwapStep {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use ruint::aliases::U256;

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn u256(s: &str) -> U256 {
        U256::from_str_radix(s, 10).unwrap()
    }

    fn ether(n: u128) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    fn liq(n: u128) -> u128 {
        n
    }

    // ── Price fixtures (from the Uniswap V3 TypeScript test suite) ────────────

    /// √(1/1) · 2^96
    fn sqrt_price_1_1() -> U256 {
        U256::ONE << 96
    }

    /// √(101/100) · 2^96
    fn sqrt_price_101_100() -> U256 {
        u256("79623317895830914510639640423")
    }

    /// √(1000/100) · 2^96 — well above sqrt_price_1_1; used as upper target
    fn sqrt_price_1000_100() -> U256 {
        u256("250541448375047925191586663628")
    }

    /// √(10000/100) · 2^96 — far above sqrt_price_1_1; used as upper target
    fn sqrt_price_10000_100() -> U256 {
        u256("792281625142643375935439503360")
    }

    /// √(1/4) · 2^96
    fn sqrt_price_1_4() -> U256 {
        u256("39614081257132168796771975168")
    }

    // ── Arbitrary-value strategies ────────────────────────────────────────────

    /// Non-zero U160 — valid range for Uniswap V3 sqrt prices.
    fn arb_sqrt_price() -> impl Strategy<Value = U256> {
        any::<[u64; 3]>()
            .prop_map(|limbs| {
                let top = limbs[2] & 0xFFFF_FFFF;
                U256::from_limbs([limbs[0], limbs[1], top, 0])
            })
            .prop_filter("price must be > 0", |v| !v.is_zero())
    }

    fn arb_u256() -> impl Strategy<Value = U256> {
        any::<[u64; 4]>().prop_map(U256::from_limbs)
    }

    // ── get_sqrt_price_target ─────────────────────────────────────────────────

    proptest! {
        #[test]
        fn fuzz_get_sqrt_price_target(
            zero_for_one in any::<bool>(),
            next  in arb_sqrt_price(),
            limit in arb_sqrt_price(),
        ) {
            let result = get_sqrt_price_target(zero_for_one, next, limit);
            let expected = if zero_for_one { next.max(limit) } else { next.min(limit) };
            prop_assert_eq!(result, expected);
        }
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[test]
    fn fee_too_large_returns_error() {
        let err = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(1_000_000),
            SignedAmount::negative(ether(1)),
            MAX_SWAP_FEE + 1,
        );
        assert_eq!(err, Err(SwapMathError::FeeTooLarge));
    }

    #[test]
    fn max_fee_exact_out_returns_error() {
        let err = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(1_000_000),
            SignedAmount::positive(ether(1)),
            MAX_SWAP_FEE,
        );
        assert_eq!(err, Err(SwapMathError::MaxFeeExactOut));
    }

    #[test]
    fn max_fee_exact_in_consumes_all_as_fee_without_price_movement() {
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(1_000_000),
            SignedAmount::negative(ether(1)),
            MAX_SWAP_FEE,
        )
        .unwrap();

        assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_1_1());
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, ether(1));
    }

    // ── Exact-input — price capped at target ──────────────────────────────────

    #[test]
    fn exact_in_one_for_zero_capped_at_price_target() {
        // Current price below target: one_for_zero (price moves up).
        // Large input; step is bounded by the tick target.
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(2_000_000_000_000_000_000),
            SignedAmount::negative(ether(1)),
            600,
        )
        .unwrap();

        assert_eq!(step.amount_in, u256("9975124224178055"));
        assert_eq!(step.amount_out, u256("9925619580021728"));
        assert_eq!(step.fee_amount, u256("5988667735148"));
        // Total spend must be strictly less than the full input.
        assert!(step.amount_in + step.fee_amount < ether(1));
        assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_101_100());
    }

    // ── Exact-output — price capped at target ─────────────────────────────────

    #[test]
    fn exact_out_one_for_zero_capped_at_price_target() {
        // Large desired output; step is bounded by the tick target.
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_101_100(),
            liq(2_000_000_000_000_000_000),
            SignedAmount::positive(ether(1)),
            600,
        )
        .unwrap();

        assert_eq!(step.amount_in, u256("9975124224178055"));
        assert_eq!(step.amount_out, u256("9925619580021728"));
        assert_eq!(step.fee_amount, u256("5988667735148"));
        assert!(step.amount_out < ether(1));
        assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_101_100());
    }

    // ── Exact-input — input fully spent before target ─────────────────────────

    #[test]
    fn exact_in_one_for_zero_fully_spent() {
        // Target price is far away; the entire input is consumed without crossing.
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_1000_100(),
            liq(2_000_000_000_000_000_000),
            SignedAmount::negative(ether(1)),
            600,
        )
        .unwrap();

        assert_eq!(step.amount_in, u256("999400000000000000"));
        assert_eq!(step.amount_out, u256("666399946655997866"));
        assert_eq!(step.fee_amount, u256("600000000000000"));
        // Net input + fee == full input amount.
        assert_eq!(step.amount_in + step.fee_amount, ether(1));
        assert!(step.sqrt_ratio_next_x96 < sqrt_price_1000_100());
    }

    // ── Exact-output — desired output fully received ──────────────────────────

    #[test]
    fn exact_out_one_for_zero_fully_received() {
        // Target is far; the desired output is fully deliverable.
        let step = compute_swap_step(
            sqrt_price_1_1(),
            sqrt_price_10000_100(),
            liq(2_000_000_000_000_000_000),
            SignedAmount::positive(ether(1)),
            600,
        )
        .unwrap();

        assert_eq!(step.amount_in, u256("2000000000000000000"));
        assert_eq!(step.fee_amount, u256("1200720432259356"));
        assert_eq!(step.amount_out, ether(1));
        assert!(step.sqrt_ratio_next_x96 < sqrt_price_10000_100());
    }

    // ── Edge: output capped at desired ────────────────────────────────────────

    #[test]
    fn amount_out_capped_at_desired() {
        let step = compute_swap_step(
            u256("417332158212080721273783715441582"),
            u256("1452870262520218020823638996"),
            159344665391607089467575320103u128,
            SignedAmount::positive(U256::ONE),
            1,
        )
        .unwrap();

        assert_eq!(step.amount_in, U256::ONE);
        assert_eq!(step.fee_amount, U256::ONE);
        assert_eq!(step.amount_out, U256::ONE);
        assert_eq!(
            step.sqrt_ratio_next_x96,
            u256("417332158212080721273783715441581")
        );
    }

    // ── Edge: target price == 1, partial input consumed ──────────────────────

    #[test]
    fn target_price_one_partial_input() {
        // An extremely small target price of 1.
        let amount = u256("3915081100057732413702495386755767");
        let step = compute_swap_step(
            U256::from(2u64),
            U256::ONE,
            liq(1),
            SignedAmount::negative(amount),
            1,
        )
        .unwrap();

        assert_eq!(step.amount_in, sqrt_price_1_4());
        assert_eq!(step.fee_amount, u256("39614120871253040049813"));
        assert!(step.amount_in + step.fee_amount <= amount);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.sqrt_ratio_next_x96, U256::ONE);
    }

    // ── Edge: not entire input taken as fee ───────────────────────────────────

    #[test]
    fn not_entire_input_taken_as_fee() {
        let step = compute_swap_step(
            U256::from(2413u64),
            u256("79887613182836312"),
            1985041575832132834610021537970u128,
            SignedAmount::negative(U256::from(10u64)),
            1872,
        )
        .unwrap();

        assert_eq!(step.amount_in, U256::from(9u64));
        assert_eq!(step.fee_amount, U256::ONE);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.sqrt_ratio_next_x96, U256::from(2413u64));
    }

    // ── Edge: insufficient liquidity exact-out (zero_for_one) ─────────────────

    #[test]
    fn zero_for_one_insufficient_liquidity_exact_out() {
        // zero_for_one: current > target  →  price moves DOWN.
        let sqrt_p = u256("20282409603651670423947251286016");
        // Target is 10 % below current; liq=1024 can only deliver ≈ 26,214 token1
        // across this range, so requesting 100_000 triggers the "step hits target" path.
        let sqrt_p_target = sqrt_p * U256::from(9u64) / U256::from(10u64);

        let step = compute_swap_step(
            sqrt_p,
            sqrt_p_target,
            liq(1024),
            SignedAmount::positive(U256::from(100_000u64)), // > max_available ≈ 26,214
            3000,
        )
        .unwrap();

        // Pool is exhausted before delivering the full request; price hits the boundary.
        assert_eq!(step.sqrt_ratio_next_x96, sqrt_p_target);
        assert!(step.amount_out <= U256::from(100_000u64));
        assert!(step.amount_out > U256::ZERO);
    }

    // ── Edge: insufficient liquidity exact-out (one_for_zero) ─────────────────

    #[test]
    fn one_for_zero_insufficient_liquidity_exact_out() {
        // one_for_zero: current < target (price moves UP).
        let sqrt_p = u256("20282409603651670423947251286016");
        // FIX: previously used sqrt_p * 9/10 (BELOW current) which contradicts
        // one_for_zero direction. Corrected to sqrt_p * 11/10 (ABOVE current).
        let sqrt_p_target = sqrt_p * U256::from(11u64) / U256::from(10u64);

        let step = compute_swap_step(
            sqrt_p,
            sqrt_p_target,
            liq(1024),
            SignedAmount::positive(U256::from(263000u64)),
            3000,
        )
        .unwrap();

        // With tiny liquidity the step reaches the target boundary.
        assert_eq!(step.sqrt_ratio_next_x96, sqrt_p_target);
        assert!(step.amount_out <= U256::from(263000u64));
    }

    // ── Property-based fuzz tests ─────────────────────────────────────────────

    proptest! {
        /// For any valid exact-input swap step:
        /// 1. `amount_in + fee_amount` does not overflow U256.
        /// 2. The total spend never exceeds `amount_remaining`.
        /// 3. The resulting price stays between `current` and `target` (inclusive).
        /// 4. If current == target, nothing moves (trivial case).
        /// 5. If price did NOT reach the target, `amount_in + fee == amount_remaining`.
        #[test]
        fn fuzz_exact_in_invariants(
            sqrt_price        in arb_sqrt_price(),
            sqrt_price_target in arb_sqrt_price(),
            liquidity         in 1u128..=u128::MAX,
            amount_remaining  in arb_u256(),
            fee_pips          in 0u32..=MAX_SWAP_FEE,
        ) {
            let result = compute_swap_step(
                sqrt_price,
                sqrt_price_target,
                liquidity,
                SignedAmount::negative(amount_remaining),
                fee_pips,
            );

            // Skip arithmetic errors (tested separately).
            let step = match result {
                Ok(s) => s,
                Err(_) => return Ok(()),
            };

            // 1. No overflow in total spend.
            prop_assert!(
                step.amount_in <= U256::MAX - step.fee_amount,
                "amount_in + fee_amount overflows U256"
            );

            // 2. Total spend ≤ remaining input.
            prop_assert!(
                step.amount_in + step.fee_amount <= amount_remaining,
                "spent more than amount_remaining"
            );

            // 3. Price stays within [current, target] (or [target, current]).
            if sqrt_price_target <= sqrt_price {
                prop_assert!(step.sqrt_ratio_next_x96 <= sqrt_price);
                prop_assert!(step.sqrt_ratio_next_x96 >= sqrt_price_target);
            } else {
                prop_assert!(step.sqrt_ratio_next_x96 >= sqrt_price);
                prop_assert!(step.sqrt_ratio_next_x96 <= sqrt_price_target);
            }

            // 4. Trivial case: current == target → nothing moves.
            if sqrt_price == sqrt_price_target {
                prop_assert_eq!(step.amount_in,  U256::ZERO);
                prop_assert_eq!(step.amount_out, U256::ZERO);
                prop_assert_eq!(step.fee_amount, U256::ZERO);
                prop_assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_target);
            }

            // 5. If price did NOT reach target, all input is fully consumed.
            if step.sqrt_ratio_next_x96 != sqrt_price_target {
                prop_assert_eq!(
                    step.amount_in + step.fee_amount,
                    amount_remaining,
                    "unfilled step did not consume full amount_remaining"
                );
            }
        }

        /// For any valid exact-output swap step:
        /// 1. `amount_in + fee_amount` does not overflow U256.
        /// 2. `amount_out ≤ amount_remaining`.
        /// 3. The resulting price stays within [current, target].
        /// 4. If price did NOT reach target, `amount_out == amount_remaining`.
        #[test]
        fn fuzz_exact_out_invariants(
            sqrt_price        in arb_sqrt_price(),
            sqrt_price_target in arb_sqrt_price(),
            liquidity         in 1u128..=u128::MAX,
            amount_remaining  in arb_u256(),
            fee_pips          in 0u32..MAX_SWAP_FEE, // strict: MAX_SWAP_FEE excluded for exact-out
        ) {
            let result = compute_swap_step(
                sqrt_price,
                sqrt_price_target,
                liquidity,
                SignedAmount::positive(amount_remaining),
                fee_pips,
            );

            let step = match result {
                Ok(s) => s,
                Err(_) => return Ok(()),
            };

            // 1. No overflow in input + fee.
            prop_assert!(
                step.amount_in <= U256::MAX - step.fee_amount,
                "amount_in + fee_amount overflows U256"
            );

            // 2. Output never exceeds the requested amount.
            prop_assert!(step.amount_out <= amount_remaining, "produced more output than requested");

            // 3. Price stays within bounds.
            if sqrt_price_target <= sqrt_price {
                prop_assert!(step.sqrt_ratio_next_x96 <= sqrt_price);
                prop_assert!(step.sqrt_ratio_next_x96 >= sqrt_price_target);
            } else {
                prop_assert!(step.sqrt_ratio_next_x96 >= sqrt_price);
                prop_assert!(step.sqrt_ratio_next_x96 <= sqrt_price_target);
            }

            // 4. Trivial case.
            if sqrt_price == sqrt_price_target {
                prop_assert_eq!(step.amount_in,  U256::ZERO);
                prop_assert_eq!(step.amount_out, U256::ZERO);
                prop_assert_eq!(step.fee_amount, U256::ZERO);
                prop_assert_eq!(step.sqrt_ratio_next_x96, sqrt_price_target);
            }

            // 5. Partial fill → exact output was delivered.
            if step.sqrt_ratio_next_x96 != sqrt_price_target {
                prop_assert_eq!(
                    step.amount_out,
                    amount_remaining,
                    "partial fill did not deliver full requested output"
                );
            }
        }
    }
}
