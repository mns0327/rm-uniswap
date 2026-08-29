use crate::core::types::fee::Fee;
use serde::{Deserialize, Serialize};

/// Maximum directional protocol fee accepted by Uniswap V4.
///
/// Protocol fees are expressed in pips against [`PIPS_DENOMINATOR`]. A value of
/// `1_000` represents 0.10%, which is the maximum protocol fee allowed for a
/// single swap direction.
pub(crate) const MAX_PROTOCOL_FEE: u16 = 1_000;

/// Denominator for fees expressed in pips.
///
/// `1_000_000` represents 100%, `10_000` represents 1%, and `1_000`
/// represents 0.10%.
pub(crate) const PIPS_DENOMINATOR: u32 = 1_000_000;

/// Directional Uniswap V4 protocol fee configuration.
///
/// Uniswap V4 stores protocol fees separately for each swap direction. The
/// `zero_for_one` fee applies when swapping token0 for token1, and the
/// `one_for_zero` fee applies when swapping token1 for token0.
///
/// Values are stored as pips in the protocol-fee domain of
/// `0..=MAX_PROTOCOL_FEE`. Use [`ProtocolFee::ZERO`] when protocol fees are
/// disabled in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProtocolFee {
    zero_for_one_fee: u16,
    one_for_zero_fee: u16,
}

impl Default for ProtocolFee {
    #[inline(always)]
    fn default() -> Self {
        Self::ZERO
    }
}

impl ProtocolFee {
    /// Protocol-fee configuration with both swap directions disabled.
    pub const ZERO: Self = Self::new(0, 0).unwrap();

    /// Attempts to construct directional protocol fees from raw pips.
    ///
    /// Returns `None` if either direction exceeds [`MAX_PROTOCOL_FEE`].
    #[inline(always)]
    pub const fn new(zero_for_one_fee: u16, one_for_zero_fee: u16) -> Option<Self> {
        let result = Self {
            zero_for_one_fee,
            one_for_zero_fee,
        };

        if result.is_valid() {
            Some(result)
        } else {
            None
        }
    }

    /// Builds directional protocol fees without validating the V4 maximum.
    ///
    /// # Safety
    ///
    /// Callers must guarantee that both directional fees are less than or equal
    /// to [`MAX_PROTOCOL_FEE`]. Invalid values can cause swap-fee calculation
    /// to fail in code paths that otherwise assume `ProtocolFee` has already
    /// been validated.
    #[inline(always)]
    pub const unsafe fn unchecked_new(zero_for_one_fee: u16, one_for_zero_fee: u16) -> Self {
        Self {
            zero_for_one_fee,
            one_for_zero_fee,
        }
    }

    /// Returns the raw protocol-fee pips for the requested swap direction.
    ///
    /// Pass `true` for token0-to-token1 swaps and `false` for token1-to-token0
    /// swaps.
    #[inline(always)]
    pub const fn pips(&self, zero_for_one: bool) -> u16 {
        if zero_for_one {
            self.zero_for_one_fee
        } else {
            self.one_for_zero_fee
        }
    }

    /// Returns `true` when both directional fees are within the V4 limit.
    #[inline(always)]
    pub const fn is_valid(&self) -> bool {
        self.zero_for_one_fee <= MAX_PROTOCOL_FEE && self.one_for_zero_fee <= MAX_PROTOCOL_FEE
    }

    /// Returns the raw protocol-fee pips for the requested swap direction.
    ///
    /// This is equivalent to [`ProtocolFee::pips`] and is kept for call sites
    /// that use the same naming as Uniswap V4's protocol-fee library.
    #[inline(always)]
    pub fn get_fee(&self, zero_for_one: bool) -> u16 {
        if zero_for_one {
            self.zero_for_one_fee
        } else {
            self.one_for_zero_fee
        }
    }

    /// Calculates the effective swap fee from protocol and LP fees.
    ///
    /// When the directional protocol fee is zero, the effective fee is exactly
    /// the pool LP fee. Otherwise, this mirrors Uniswap V4's
    /// `ProtocolFeeLibrary.calculateSwapFee` formula:
    ///
    /// ```text
    /// protocolFee + lpFee - floor(protocolFee * lpFee / PIPS_DENOMINATOR)
    /// ```
    ///
    /// The final term removes the overlap between protocol and LP fees so the
    /// result represents a single effective fee applied to the swap input.
    pub(crate) fn calculate_swap_fee(
        &self,
        zero_for_one: bool,
        lp_fee: &Fee,
    ) -> Result<Fee, crate::Error> {
        let protocol_fee = self.get_fee(zero_for_one);

        if protocol_fee == 0 {
            return Ok(*lp_fee);
        }

        // Match V4's `calculateSwapFee` rounding by flooring the overlap term.
        let overlap =
            (u64::from(protocol_fee) * u64::from(lp_fee.pips())) / u64::from(PIPS_DENOMINATOR);
        let combined = u64::from(protocol_fee) + u64::from(lp_fee.pips()) - overlap;

        Fee::new(combined as u32).ok_or(crate::Error::FeeTooLarge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fee(pips: u32) -> Fee {
        Fee::new(pips).unwrap()
    }

    #[test]
    fn zero_disables_both_directions() {
        assert_eq!(ProtocolFee::ZERO.pips(true), 0);
        assert_eq!(ProtocolFee::ZERO.pips(false), 0);
        assert!(ProtocolFee::ZERO.is_valid());
        assert_eq!(ProtocolFee::default(), ProtocolFee::ZERO);
    }

    #[test]
    fn new_accepts_protocol_fee_bounds() {
        let protocol_fee = ProtocolFee::new(MAX_PROTOCOL_FEE, MAX_PROTOCOL_FEE).unwrap();

        assert_eq!(protocol_fee.pips(true), MAX_PROTOCOL_FEE);
        assert_eq!(protocol_fee.pips(false), MAX_PROTOCOL_FEE);
        assert!(protocol_fee.is_valid());
    }

    #[test]
    fn new_rejects_values_above_v4_maximum() {
        assert!(ProtocolFee::new(MAX_PROTOCOL_FEE + 1, 0).is_none());
        assert!(ProtocolFee::new(0, MAX_PROTOCOL_FEE + 1).is_none());
    }

    #[test]
    fn pips_and_get_fee_return_directional_values() {
        let protocol_fee = ProtocolFee::new(123, 456).unwrap();

        assert_eq!(protocol_fee.pips(true), 123);
        assert_eq!(protocol_fee.pips(false), 456);
        assert_eq!(protocol_fee.get_fee(true), 123);
        assert_eq!(protocol_fee.get_fee(false), 456);
    }

    #[test]
    fn calculate_swap_fee_returns_lp_fee_when_protocol_fee_is_zero() {
        let protocol_fee = ProtocolFee::new(0, 500).unwrap();
        let lp_fee = fee(3_000);

        assert_eq!(protocol_fee.calculate_swap_fee(true, &lp_fee), Ok(lp_fee));
    }

    #[test]
    fn calculate_swap_fee_uses_requested_direction() {
        let protocol_fee = ProtocolFee::new(100, 500).unwrap();
        let lp_fee = fee(3_000);

        assert_eq!(
            protocol_fee
                .calculate_swap_fee(true, &lp_fee)
                .unwrap()
                .pips(),
            3_100
        );
        assert_eq!(
            protocol_fee
                .calculate_swap_fee(false, &lp_fee)
                .unwrap()
                .pips(),
            3_499
        );
    }

    #[test]
    fn calculate_swap_fee_floors_protocol_lp_overlap() {
        let protocol_fee = ProtocolFee::new(333, 500).unwrap();
        let lp_fee = fee(3_000);

        assert_eq!(
            protocol_fee
                .calculate_swap_fee(true, &lp_fee)
                .unwrap()
                .pips(),
            3_333
        );
        assert_eq!(
            protocol_fee
                .calculate_swap_fee(false, &lp_fee)
                .unwrap()
                .pips(),
            3_499
        );
    }

    #[test]
    fn calculate_swap_fee_preserves_max_lp_fee() {
        let protocol_fee = ProtocolFee::new(MAX_PROTOCOL_FEE, MAX_PROTOCOL_FEE).unwrap();
        let lp_fee = fee(PIPS_DENOMINATOR);

        assert_eq!(
            protocol_fee.calculate_swap_fee(true, &lp_fee),
            Ok(fee(PIPS_DENOMINATOR))
        );
        assert_eq!(
            protocol_fee.calculate_swap_fee(false, &lp_fee),
            Ok(fee(PIPS_DENOMINATOR))
        );
    }

    #[test]
    fn unchecked_new_skips_validation() {
        let protocol_fee =
            unsafe { ProtocolFee::unchecked_new(MAX_PROTOCOL_FEE + 1, MAX_PROTOCOL_FEE + 2) };

        assert_eq!(protocol_fee.pips(true), MAX_PROTOCOL_FEE + 1);
        assert_eq!(protocol_fee.pips(false), MAX_PROTOCOL_FEE + 2);
        assert!(!protocol_fee.is_valid());
    }
}
