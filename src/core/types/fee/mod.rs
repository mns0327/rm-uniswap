use serde::{Deserialize, Serialize};

pub mod protocol_fee;
pub mod swap_fee;

/// Maximum swap fee accepted by Uniswap swap math.
///
/// Fees are expressed in pips, also called hundredths of a bip:
/// `1_000_000` represents 100%, `10_000` represents 1%, and `3_000`
/// represents 0.30%.
pub(crate) const MAX_FEE: u32 = 1_000_000;

/// A validated Uniswap swap fee stored in pips.
///
/// Wrapping a raw `u32` in this type guarantees that any `Fee` in circulation
/// is within the protocol fee domain of `0..=1_000_000`. The complement
/// (`1_000_000 - pips`) is cached next to the raw value because swap math uses
/// it repeatedly on the hot path.
///
/// Serialization stays wire-compatible with existing pool snapshots: a `Fee`
/// encodes as the raw pips integer and rejects out-of-range values while
/// decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fee {
    pips: u32,
    complement: u32,
}

impl Fee {
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
    #[must_use]
    pub fn pips(self) -> u32 {
        self.pips
    }

    /// Returns `MAX_FEE - self.pips()`, the fraction left after swap fees.
    ///
    /// Exact-input math multiplies the caller's input by this value before
    /// dividing by `MAX_FEE`.
    #[inline(always)]
    #[must_use]
    pub fn complement(self) -> u32 {
        self.complement
    }

    /// Returns `true` when no swap fee is charged.
    #[inline(always)]
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.pips == 0
    }

    /// Returns `true` when the fee is 100%.
    #[inline(always)]
    #[must_use]
    pub const fn is_max(self) -> bool {
        self.pips == MAX_FEE
    }
}

impl Serialize for Fee {
    /// Serializes `Fee` as the raw pips integer for snapshot compatibility.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.pips.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Fee {
    /// Deserializes raw pips and validates the value before constructing `Fee`.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let pips = u32::deserialize(deserializer)?;
        Self::new(pips).ok_or(serde::de::Error::custom("invalid fee pips"))
    }
}
