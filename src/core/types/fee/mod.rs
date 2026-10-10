#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

pub mod protocol_fee;
pub mod swap_fee;

/// Maximum swap fee accepted by Uniswap swap math.
///
/// Fees are expressed in pips, also called hundredths of a bip:
/// `1_000_000` represents 100%, `10_000` represents 1%, and `3_000`
/// represents 0.30%.
pub const MAX_FEE: u32 = 1_000_000;

/// Sentinel `PoolKey.fee` value marking a pool as dynamic-fee in Uniswap V4.
///
/// Mirrors V4's `LPFeeLibrary.DYNAMIC_FEE_FLAG`. It is greater than
/// the `1_000_000` max fee, so it can never collide with a valid static fee: pools compare
/// their raw `fee` against this constant to decide whether the before-swap
/// hook is allowed to override the LP fee for that swap.
pub const DYNAMIC_FEE_FLAG: u32 = 0x800000;
const _: () = assert!(DYNAMIC_FEE_FLAG > MAX_FEE);

/// A validated Uniswap swap fee stored in pips.
///
/// Wrapping a raw `u32` in this type guarantees that any `Fee` in circulation
/// is within the protocol fee domain of `0..=1_000_000`, with one exception:
/// [`Fee::DYNAMIC`] carries [`DYNAMIC_FEE_FLAG`] as a placeholder for
/// dynamic-fee pools and is never valid swap-math input. Use [`Fee::is_valid`]
/// to tell the two apart. The complement (`1_000_000 - pips`) is cached next
/// to the raw value because swap math uses it repeatedly on the hot path.
///
/// Serialization stays wire-compatible with existing pool snapshots: a `Fee`
/// encodes as the raw pips integer and rejects out-of-range values while
/// decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fee {
    pips: u32,
    complement: u32,
}

impl Fee {
    /// No swap fee charged.
    pub const ZERO: Self = Self::new(0).unwrap();

    /// Placeholder `Fee` carrying [`DYNAMIC_FEE_FLAG`] instead of a static
    /// value.
    ///
    /// Stands in for the LP fee of a dynamic-fee pool before its before-swap
    /// hook has supplied a fee for the current swap. [`Fee::is_valid`] is
    /// `false` for this value, so it is rejected wherever swap math expects a
    /// resolved fee.
    pub const DYNAMIC: Self = Self {
        pips: DYNAMIC_FEE_FLAG,
        complement: 0,
    };

    /// Attempts to construct a `Fee` from raw pips.
    ///
    /// Returns `None` if `fee_pips` is greater than `MAX_FEE`. A value equal
    /// to `MAX_FEE` is valid for exact-input swaps, but exact-output swaps
    /// reject it at the pool layer because no finite input can both pay a
    /// 100% fee and deliver output.
    #[inline(always)]
    pub const fn new(fee_pips: u32) -> Option<Self> {
        if fee_pips > MAX_FEE {
            return None;
        }

        Some(Self {
            pips: fee_pips,
            complement: MAX_FEE - fee_pips,
        })
    }

    /// Returns the raw fee in pips.
    #[inline(always)]
    pub const fn pips(&self) -> u32 {
        self.pips
    }

    /// Returns `MAX_FEE - self.pips()`, the fraction left after swap fees.
    ///
    /// Exact-input math multiplies the caller's input by this value before
    /// dividing by `MAX_FEE`.
    #[inline(always)]
    pub const fn complement(&self) -> u32 {
        self.complement
    }

    /// Returns `true` when no swap fee is charged.
    #[inline(always)]
    pub const fn is_zero(&self) -> bool {
        self.pips == 0
    }

    /// Returns `true` when the fee is 100%.
    #[inline(always)]
    pub const fn is_max(&self) -> bool {
        self.pips == MAX_FEE
    }

    /// Returns `true` when `self` is a valid static fee usable in swap math.
    ///
    /// [`Fee::DYNAMIC`] is the only value constructible through this type's
    /// public API that fails this check.
    #[inline(always)]
    pub const fn is_valid(&self) -> bool {
        self.pips <= MAX_FEE
    }

    /// Returns `true` when `self` is the dynamic-fee placeholder.
    #[inline(always)]
    pub const fn is_dynamic_fee(&self) -> bool {
        self.pips == DYNAMIC_FEE_FLAG
    }
}

#[cfg(feature = "serde")]
impl Serialize for Fee {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.pips.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for Fee {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let pips = u32::deserialize(deserializer)?;
        Self::new(pips).ok_or(serde::de::Error::custom("invalid fee pips"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_max_fee_for_exact_input() {
        let fee = Fee::new(MAX_FEE).unwrap();

        assert_eq!(fee.pips(), MAX_FEE);
        assert_eq!(fee.complement(), 0);
        assert!(fee.is_max());
        assert!(fee.is_valid());
    }

    #[test]
    fn new_rejects_values_above_max_fee() {
        assert!(Fee::new(MAX_FEE + 1).is_none());
    }

    #[test]
    fn zero_is_the_no_fee_case() {
        assert!(Fee::ZERO.is_zero());
        assert_eq!(Fee::ZERO.pips(), 0);
        assert_eq!(Fee::ZERO.complement(), MAX_FEE);
    }

    #[test]
    fn complement_pairs_with_pips_for_valid_fees() {
        let fee = Fee::new(300_000).unwrap();

        assert_eq!(fee.pips() + fee.complement(), MAX_FEE);
    }

    #[test]
    fn dynamic_marker_is_flagged_but_not_a_valid_static_fee() {
        assert!(Fee::DYNAMIC.is_dynamic_fee());
        assert!(!Fee::DYNAMIC.is_valid());
        assert_eq!(Fee::DYNAMIC.pips(), DYNAMIC_FEE_FLAG);
    }

    #[test]
    fn is_dynamic_fee_is_false_for_static_fees() {
        assert!(!Fee::ZERO.is_dynamic_fee());
        assert!(!Fee::new(MAX_FEE).unwrap().is_dynamic_fee());
    }

    #[test]
    #[cfg(feature = "serde")]
    fn serde_json_is_transparent_number() {
        let fee = Fee::new(3_000).unwrap();
        let json = serde_json::to_string(&fee).unwrap();
        assert_eq!(json, "3000");

        let decoded: Fee = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, fee);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn serde_json_rejects_out_of_range_pips() {
        let result: Result<Fee, _> = serde_json::from_str(&(MAX_FEE + 1).to_string());
        assert!(result.is_err());
    }
}
