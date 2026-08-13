use serde::{Deserialize, Serialize};

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
    /// to `MAX_FEE` is valid for exact-input swaps, but exact-output callers
    /// must reject it with `validate_swap_fee_for_exactness`.
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

/// Validates the swap fee against the swap exactness.
///
/// V4 allows a 100% fee for exact-input swaps: the entire caller input can be
/// consumed as fee and produce no output. Exact-output swaps must reject 100%
/// fees because no finite input can both pay the fee and deliver output.
#[inline]
pub(crate) fn validate_swap_fee_for_exactness(
    fee: Fee,
    exact_in: bool,
) -> Result<(), crate::Error> {
    if !exact_in && fee.is_max() {
        Err(crate::Error::FeeTooLarge)
    } else {
        Ok(())
    }
}

/// Helpers for combining the pool LP fee with an optional V4 protocol fee.
///
/// Protocol fees are configured per swap and are applied before LP fee growth
/// accounting. The combined effective fee follows V4's overlap formula rather
/// than simply adding both percentages.
#[cfg(feature = "protocol-fee")]
pub(crate) mod protocol {
    use super::Fee;

    /// `ProtocolFeeLibrary.MAX_PROTOCOL_FEE` = 1 000 (0.1%).
    pub(crate) const MAX_PROTOCOL_FEE: u32 = 1_000;
    /// `ProtocolFeeLibrary.PIPS_DENOMINATOR` = 1 000 000.
    pub(crate) const PIPS_DENOMINATOR: u32 = 1_000_000;

    /// Calculates the effective swap fee from protocol and LP fees.
    ///
    /// When no protocol fee is supplied, this returns the pool LP fee. When a
    /// protocol fee is present, V4 combines the two fees as:
    ///
    /// ```text
    /// protocolFee + lpFee - floor(protocolFee * lpFee / PIPS_DENOMINATOR)
    /// ```
    ///
    /// The subtraction removes the overlap between the two percentages so the
    /// combined fee still represents one effective percentage of the input.
    pub(crate) fn calculate_swap_fee(
        protocol_fee: Option<u32>,
        lp_fee: Fee,
    ) -> Result<Fee, crate::Error> {
        let Some(protocol_fee) = protocol_fee else {
            return Ok(lp_fee);
        };

        if protocol_fee > MAX_PROTOCOL_FEE {
            return Err(crate::Error::FeeTooLarge);
        }
        if protocol_fee == 0 {
            return Ok(lp_fee);
        }

        // V4's `calculateSwapFee`:
        //   protocolFee + lpFee - floor(protocolFee * lpFee / PIPS_DENOMINATOR)
        let overlap =
            (u64::from(protocol_fee) * u64::from(lp_fee.pips)) / u64::from(PIPS_DENOMINATOR);
        let combined = u64::from(protocol_fee) + u64::from(lp_fee.pips) - overlap;

        if combined > u64::from(PIPS_DENOMINATOR) {
            return Err(crate::Error::FeeTooLarge);
        }

        Fee::new(combined as u32).ok_or(crate::Error::FeeTooLarge)
    }
}
