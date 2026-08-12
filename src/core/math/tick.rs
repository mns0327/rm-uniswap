//! Tick-to-price conversion for the Uniswap tick grid.
//!
//! The constants and rounding rules mirror the canonical Uniswap TickMath
//! library. The public functions operate on validated domain types so callers
//! can keep protocol bounds at the API edge.

use ruint::aliases::{U160, U256};

use crate::core::types::{sqrt_price::SqrtPriceX96, tick::TickIndex};

/// Error type retained for TickMath-compatible helper APIs.
pub type TickMathError = crate::Error;

// Multipliers from Uniswap TickMath.sol, encoded as Q128 fixed-point values.
// Entry k is floor(2^128 / sqrt(1.0001^(2^k))). Applying the entries for each
// set bit in abs(tick) reconstructs sqrt(1.0001^tick) before final Q64.96
// rounding.
const TICK_RATIOS: [u128; 20] = [
    0xfffcb933bd6fad37aa2d162d1a594001_u128,
    0xfff97272373d413259a46990580e213a_u128,
    0xfff2e50f5f656932ef12357cf3c7fdcc_u128,
    0xffe5caca7e10e4e61c3624eaa0941cd0_u128,
    0xffcb9843d60f6159c9db58835c926644_u128,
    0xff973b41fa98c081472e6896dfb254c0_u128,
    0xff2ea16466c96a3843ec78b326b52861_u128,
    0xfe5dee046a99a2a811c461f1969c3053_u128,
    0xfcbe86c7900a88aedcffc83b479aa3a4_u128,
    0xf987a7253ac413176f2b074cf7815e54_u128,
    0xf3392b0822b70005940c7a398e4b70f3_u128,
    0xe7159475a2c29b7443b29c7fa6e889d9_u128,
    0xd097f3bdfd2022b8845ad8f792aa5825_u128,
    0xa9f746462d870fdf8a65dc1f90e061e5_u128,
    0x70d869a156d2a1b890bb3df62baf32f7_u128,
    0x31be135f97d08fd981231505542fcfa6_u128,
    0x09aa508b5b7a84e1c677de54f3e99bc9_u128,
    0x005d6af8dedb81196699c329225ee604_u128,
    0x0002216e584f5fa1ea926041bedfe98_u128,
    0x00000000048a170391f7dc42444e8fa2_u128,
];

/// Return floor(a * b / 2^128).
///
/// This is the high half of the 256-bit product. It is enough for the TickMath
/// Q128 recurrence and avoids building the lower half that will be discarded.
#[inline(always)]
const fn mul_hi_u128(a: u128, b: u128) -> u128 {
    const LO: u128 = u64::MAX as u128;
    let (al, ah) = (a & LO, a >> 64);
    let (bl, bh) = (b & LO, b >> 64);

    let ll = al * bl;
    let lh = al * bh;
    let hl = ah * bl;
    let hh = ah * bh;

    let mid = (ll >> 64) + (lh & LO) + (hl & LO);
    hh + (lh >> 64) + (hl >> 64) + (mid >> 64)
}

/// Return the Q64.96 square-root price for a validated tick.
///
/// The result is equivalent to Uniswap's `getSqrtRatioAtTick`: it represents
/// `sqrt(1.0001^tick) * 2^96`, rounded according to the reference library so
/// that the inverse conversion remains consistent at tick boundaries.
#[inline]
pub fn get_sqrt_price_at_tick(tick_idx: TickIndex) -> SqrtPriceX96 {
    let abs_tick = tick_idx.unsigned_abs();

    if abs_tick == 0 {
        return unsafe { SqrtPriceX96::new_unchecked(U160::ONE << 96) };
    }

    let mut bits = abs_tick;
    let first = bits.trailing_zeros() as usize;
    let mut ratio = TICK_RATIOS[first];
    bits &= bits - 1;

    while bits != 0 {
        let i = bits.trailing_zeros() as usize;
        ratio = mul_hi_u128(ratio, TICK_RATIOS[i]);
        bits &= bits - 1;
    }

    if *tick_idx > 0 {
        let ratio = U256::MAX / U256::from(ratio);
        let result: U256 = (ratio + ((U256::ONE << 32) - U256::ONE)) >> 32;
        unsafe { SqrtPriceX96::new_unchecked(result.to::<U160>()) }
    } else {
        const Q32_MASK: u128 = (1u128 << 32) - 1;
        let result = (ratio >> 32) + ((ratio & Q32_MASK != 0) as u128);
        unsafe { SqrtPriceX96::new_unchecked(U160::from(result)) }
    }
}

/// Return the full 256-bit product of two `u128` values as `(high, low)`.
///
/// The pair represents `high * 2^128 + low`. This helper keeps the inverse
/// logarithm loop in native integer arithmetic while preserving exact
/// intermediate products.
#[inline(always)]
const fn widening_mul_u128(a: u128, b: u128) -> (u128, u128) {
    const LO: u128 = u64::MAX as u128;
    let (al, ah) = (a & LO, a >> 64);
    let (bl, bh) = (b & LO, b >> 64);

    let ll = al * bl;
    let lh = al * bh;
    let hl = ah * bl;
    let hh = ah * bh;

    let mid = (ll >> 64) + (lh & LO) + (hl & LO);

    let lo = ((mid & LO) << 64) | (ll & LO);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
    (hi, lo)
}
/// Multiply a signed `i128` by a `u128` and return a signed high half plus low half.
///
/// The result represents `high * 2^128 + low`. The caller only uses ranges
/// where the signed high half fits in `i64`.
#[inline(always)]
const fn smul_i128_u128(a: i128, b: u128) -> (i64, u128) {
    let neg = a < 0;
    let a_mag = if neg { a.unsigned_abs() } else { a as u128 };
    let (hi, lo) = widening_mul_u128(a_mag, b);
    if neg {
        if lo == 0 {
            (-(hi as i64), 0)
        } else {
            (-(hi as i64) - 1, lo.wrapping_neg())
        }
    } else {
        (hi as i64, lo)
    }
}
/// Return the greatest tick whose square-root price is less than or equal to `sqrt_price_x96`.
///
/// This is the inverse of [`get_sqrt_price_at_tick`] over the validated
/// `SqrtPriceX96` domain. The final candidate check preserves the same floor
/// semantics as the Uniswap reference implementation.
#[inline]
pub fn get_tick_at_sqrt_price(sqrt_price_x96: &SqrtPriceX96) -> TickIndex {
    // Shift Q64.96 input into Q128.128 before extracting a binary logarithm.
    let ratio = sqrt_price_x96.as_u256() << 32u32;
    let msb = (255u32 - ratio.leading_zeros() as u32) as i32;

    let mut log2: i128 = ((msb as i128) - 128) << 64;

    // Normalize the ratio into [2^127, 2^128) so the refinement loop can run
    // on a single `u128` value.
    let r_u256: U256 = if msb >= 128 {
        ratio >> (msb - 127) as u32
    } else {
        ratio << (127 - msb) as u32
    };
    let limbs = r_u256.as_limbs();
    let mut r: u128 = (limbs[0] as u128) | ((limbs[1] as u128) << 64);

    // Refine the fractional log2 bits using the same square-and-normalize
    // method as the Solidity implementation.
    for i in (50u32..=63u32).rev() {
        let (hi, lo) = widening_mul_u128(r, r);
        let f = (hi >> 127) as i128;
        log2 |= f << i;
        r = if f != 0 { hi } else { (hi << 1) | (lo >> 127) };
    }

    const LOG_MUL: u128 = 255_738_958_999_603_826_347_141;
    let (log_hi, log_lo) = smul_i128_u128(log2, LOG_MUL);

    // The offsets bracket the exact log conversion. The final price comparison
    // below resolves the at-most-one-tick ambiguity.
    const TICK_LOW_OFFSET: u128 = 3_402_992_956_809_132_418_596_140_100_660_247_210;
    const TICK_HIGH_OFFSET: u128 = 291_339_464_771_989_622_907_027_621_153_398_088_495;

    let (_, borrow) = log_lo.overflowing_sub(TICK_LOW_OFFSET);
    let tick_low = clamp_i64_to_tick_index(log_hi - borrow as i64);

    let (_, carry) = log_lo.overflowing_add(TICK_HIGH_OFFSET);
    let tick_high = clamp_i64_to_tick_index(log_hi + carry as i64);
    let tick = if tick_low == tick_high {
        tick_low
    } else if &get_sqrt_price_at_tick(tick_high) <= sqrt_price_x96 {
        tick_high
    } else {
        tick_low
    };

    tick
}

#[inline(always)]
fn clamp_i64_to_tick_index(tick: i64) -> TickIndex {
    let clamped = tick.clamp(
        i64::from(TickIndex::MIN.value()),
        i64::from(TickIndex::MAX.value()),
    );
    TickIndex::new(clamped as i32).expect("clamped tick must fit TickIndex")
}

// Unit tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sqrt_price_x96, tick_idx};

    type S256 = (bool, U256);

    fn sadd(a: S256, b: S256) -> S256 {
        if a.0 == b.0 {
            (a.0, a.1 + b.1)
        } else if a.1 >= b.1 {
            (a.0, a.1 - b.1)
        } else {
            (b.0, b.1 - a.1)
        }
    }

    fn ssub(a: S256, b: S256) -> S256 {
        sadd(a, (!b.0, b.1))
    }

    fn ssar(val: S256, shift: u32) -> i32 {
        let (neg, mag) = val;
        let quotient = (mag >> shift).as_limbs()[0] as i64;
        let has_remainder = !(mag & ((U256::ONE << shift) - U256::ONE)).is_zero();
        if neg {
            (-(quotient + has_remainder as i64)) as i32
        } else {
            quotient as i32
        }
    }

    #[test]
    fn sqrt_price_at_min_tick_matches_uniswap() {
        assert_eq!(get_sqrt_price_at_tick(TickIndex::MIN), SqrtPriceX96::MIN);
    }

    #[test]
    fn sqrt_price_at_max_tick_matches_uniswap() {
        assert_eq!(get_sqrt_price_at_tick(TickIndex::MAX), SqrtPriceX96::MAX);
    }

    #[test]
    fn sqrt_price_at_tick_zero_is_2_pow_96() {
        assert_eq!(
            get_sqrt_price_at_tick(tick_idx!(0)),
            SqrtPriceX96::new(U160::ONE << 96).unwrap()
        );
    }

    #[test]
    fn tick_at_min_sqrt_price_returns_min_tick() {
        assert_eq!(get_tick_at_sqrt_price(&SqrtPriceX96::MIN), TickIndex::MIN);
    }

    #[test]
    fn tick_at_sqrt_price_of_tick_zero_round_trips() {
        let sqrt_price = get_sqrt_price_at_tick(TickIndex::new(0).unwrap());
        assert_eq!(
            get_tick_at_sqrt_price(&sqrt_price),
            TickIndex::new(0).unwrap()
        );
    }

    /// Representative round trips across the protocol tick range.
    #[test]
    fn tick_round_trip_samples() {
        let ticks = [
            TickIndex::MIN,
            tick_idx!(-500_000),
            tick_idx!(-100_000),
            tick_idx!(-1),
            tick_idx!(0),
            tick_idx!(1),
            tick_idx!(100_000),
            tick_idx!(500_000),
            tick_idx!(TickIndex::MAX.value() - 1),
        ];
        for tick in ticks {
            let sqrt_price = get_sqrt_price_at_tick(tick);
            let recovered = get_tick_at_sqrt_price(&sqrt_price);
            assert_eq!(recovered, tick, "round-trip failed for tick={tick}");
        }
    }

    // Keep helper arithmetic covered with values that exercise carries.
    #[test]
    fn widening_mul_identity() {
        let base: u128 = 1u128 << 127;
        let (hi, lo) = widening_mul_u128(base, base);
        assert_eq!(lo, 0);
        assert_eq!(hi, 1u128 << 126);
    }

    #[test]
    fn widening_mul_max() {
        let (hi, lo) = widening_mul_u128(u128::MAX, u128::MAX);
        assert_eq!(hi, u128::MAX - 1);
        assert_eq!(lo, 1);
    }

    // Negative products use a two's-complement `(high, low)` representation.
    #[test]
    fn smul_negative() {
        let (hi, lo) = smul_i128_u128(-1, 1u128 << 64);
        assert_eq!(hi, -1i64);
        assert_eq!(lo, u128::MAX - (1u128 << 64) + 1);
    }

    // Exact tick prices should round trip through the inverse conversion.
    #[test]
    fn round_trip_spot_checks() {
        for &tick in &[
            tick_idx!(-887272i32),
            tick_idx!(-100_000),
            tick_idx!(-1),
            tick_idx!(0),
            tick_idx!(1),
            tick_idx!(100_000),
            tick_idx!(887271),
        ] {
            let sqrt_price = get_sqrt_price_at_tick(tick);
            let recovered = get_tick_at_sqrt_price(&sqrt_price);
            assert!(
                recovered == tick || recovered == TickIndex::new(tick.value() - 1).unwrap(),
                "tick={tick}, recovered={recovered}"
            );
        }
    }

    /// Dense coverage around boundary regions where rounding mistakes are most visible.
    #[test]
    fn tick_round_trip_dense_boundary_regions() {
        let regions: &[(TickIndex, TickIndex)] = &[
            (TickIndex::MIN, tick_idx!(TickIndex::MIN.value() + 200)),
            (tick_idx!(-1_000), tick_idx!(1_000)),
            (
                tick_idx!(TickIndex::MAX.value() - 200),
                tick_idx!(TickIndex::MAX.value() - 1),
            ),
        ];
        for &(lo, hi) in regions {
            for tick in lo.value()..=hi.value() {
                let tick = tick_idx!(tick);
                let sqrt_price = get_sqrt_price_at_tick(tick);
                let recovered = get_tick_at_sqrt_price(&sqrt_price);
                assert_eq!(recovered, tick, "dense round-trip failed for tick={tick}");
            }
        }
    }

    // Reference vectors generated from the canonical Uniswap TickMath implementation.

    #[test]
    fn uniswap_v3_reference_vectors_sqrt_price_at_tick() {
        let cases: &[(TickIndex, &str)] = &[
            (tick_idx!(-887_272), "4295128739"),
            (
                tick_idx!(887_272),
                "1461446703485210103287273052203988822378723970342",
            ),
            (tick_idx!(0), "79228162514264337593543950336"),
            (tick_idx!(1), "79232123823359799118286999568"),
            (tick_idx!(10), "79267784519130042428790663799"),
            (tick_idx!(100), "79625275426524748796330556128"),
            (tick_idx!(1_000), "83290069058676223003182343270"),
            (tick_idx!(10_000), "130621891405341611593710811006"),
            (tick_idx!(50_000), "965075977353221155028623082916"),
            (tick_idx!(-1), "79224201403219477170569942574"),
            (tick_idx!(-10), "79188560314459151373725315960"),
            (tick_idx!(-100), "78833030112140176575862854579"),
            (tick_idx!(-1_000), "75364347830767020784054125655"),
            (tick_idx!(-887_270), "4295558252"),
            (
                tick_idx!(887_270),
                "1461300573427867316570072651998408279850435624081",
            ),
            (tick_idx!(-887_220), "4306310044"),
            (
                tick_idx!(887_220),
                "1457652066949847389969617340386294118487833376468",
            ),
        ];

        for &(tick, expected_str) in cases {
            let expected =
                SqrtPriceX96::new(U160::from_str_radix(expected_str, 10).unwrap()).unwrap();
            let got = get_sqrt_price_at_tick(tick);
            assert_eq!(
                got, expected,
                "getSqrtPriceAtTick({tick}): got {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn uniswap_v3_reference_vectors_tick_at_sqrt_price() {
        let cases: &[(SqrtPriceX96, TickIndex)] = &[
            (sqrt_price_x96!(4295128739), tick_idx!(-887_272)),
            (sqrt_price_x96!(79228162514264337593543950336), tick_idx!(0)),
            (sqrt_price_x96!(79232123823359799118286999568), tick_idx!(1)),
            (
                sqrt_price_x96!(79267784519130042428790663799),
                tick_idx!(10),
            ),
            (
                sqrt_price_x96!(79625275426524748796330556128),
                tick_idx!(100),
            ),
            (
                sqrt_price_x96!(83290069058676223003182343270),
                tick_idx!(1_000),
            ),
            (
                sqrt_price_x96!(130621891405341611593710811006),
                tick_idx!(10_000),
            ),
            (
                sqrt_price_x96!(965075977353221155028623082916),
                tick_idx!(50_000),
            ),
            (
                sqrt_price_x96!(79224201403219477170569942574),
                tick_idx!(-1),
            ),
            (
                sqrt_price_x96!(79188560314459151373725315960),
                tick_idx!(-10),
            ),
            (
                sqrt_price_x96!(78833030112140176575862854579),
                tick_idx!(-100),
            ),
            (
                sqrt_price_x96!(75364347830767020784054125655),
                tick_idx!(-1_000),
            ),
            (sqrt_price_x96!(4295558252), tick_idx!(-887_270)),
            (
                sqrt_price_x96!(1461300573427867316570072651998408279850435624081),
                tick_idx!(887_270),
            ),
            (sqrt_price_x96!(4306310044), tick_idx!(-887_220)),
            (
                sqrt_price_x96!(1457652066949847389969617340386294118487833376468),
                tick_idx!(887_220),
            ),
        ];

        for &(sqrt_price, expected_tick) in cases {
            let got = get_tick_at_sqrt_price(&sqrt_price);
            assert_eq!(
                got, expected_tick,
                "getTickAtSqrtPrice({sqrt_price}): got {got}, expected {expected_tick}"
            );
        }
    }

    /// Small ticks exercise the low-bit multiplier path used by the forward conversion.
    #[test]
    fn early_exit_correctness_small_ticks() {
        for tick in [
            tick_idx!(-255),
            tick_idx!(-128),
            tick_idx!(-64),
            tick_idx!(-1),
            tick_idx!(1),
            tick_idx!(64),
            tick_idx!(128),
            tick_idx!(255),
        ] {
            let sqrt_price = get_sqrt_price_at_tick(tick);
            let recovered = get_tick_at_sqrt_price(&sqrt_price);
            assert_eq!(
                recovered, tick,
                "early-exit round-trip failed for tick={tick}"
            );
        }
    }

    #[test]
    fn ssar_positive_no_remainder() {
        assert_eq!(ssar((false, U256::from(256u64)), 3), 32);
    }

    #[test]
    fn ssar_positive_with_remainder() {
        assert_eq!(ssar((false, U256::from(9u64)), 3), 1);
    }

    #[test]
    fn ssar_negative_no_remainder() {
        assert_eq!(ssar((true, U256::from(256u64)), 3), -32);
    }

    #[test]
    fn ssar_negative_with_remainder() {
        assert_eq!(ssar((true, U256::from(9u64)), 3), -2);
    }

    #[test]
    fn sadd_same_sign() {
        let a: S256 = (false, U256::from(10u64));
        let b: S256 = (false, U256::from(5u64));
        assert_eq!(sadd(a, b), (false, U256::from(15u64)));
    }

    #[test]
    fn sadd_different_sign_positive_dominant() {
        let a: S256 = (false, U256::from(10u64));
        let b: S256 = (true, U256::from(3u64));
        assert_eq!(sadd(a, b), (false, U256::from(7u64)));
    }

    #[test]
    fn ssub_basic() {
        let a: S256 = (false, U256::from(10u64));
        let b: S256 = (false, U256::from(15u64));
        assert_eq!(ssub(a, b), (true, U256::from(5u64)));
    }
}
