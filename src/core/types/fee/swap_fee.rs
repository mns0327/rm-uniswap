#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::core::types::fee::{Fee, protocol_fee::ProtocolFee};

/// Effective swap-fee configuration for a pool.
///
/// Uniswap V4 applies LP and protocol fees as one effective input fee, but the
/// protocol component can differ by swap direction. This type stores the raw
/// LP fee and protocol-fee configuration, then caches the calculated effective
/// fees for both token0-to-token1 and token1-to-token0 swaps.
///
/// `lp_fee` is `None` for a dynamic-fee pool that has not yet had its fee
/// resolved by a before-swap hook: both cached effective fees are
/// [`Fee::ZERO`] until [`SwapFee::set_lp_fee`] supplies one.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SwapFee {
    lp_fee: Option<Fee>,
    protocol_fee: ProtocolFee,
    zero_for_one_swap_fee: Fee,
    one_for_zero_swap_fee: Fee,
}

impl SwapFee {
    /// Builds a static-fee swap-fee cache from the pool LP fee and protocol fees.
    ///
    /// Returns an error if either directional effective fee exceeds the V4
    /// swap-fee limit. Use [`SwapFee::new_dynamic`] for dynamic-fee pools.
    #[inline]
    pub fn new(lp_fee: Fee, protocol_fee: ProtocolFee) -> Result<Self, crate::Error> {
        Self::from_lp_fee(Some(lp_fee), protocol_fee)
    }

    /// Builds the swap-fee cache of a dynamic-fee pool.
    ///
    /// The LP fee starts unresolved and both effective fees are [`Fee::ZERO`]
    /// until a before-swap hook override or [`SwapFee::set_lp_fee`] supplies one.
    #[inline]
    pub fn new_dynamic(protocol_fee: ProtocolFee) -> Self {
        Self {
            lp_fee: None,
            protocol_fee,
            zero_for_one_swap_fee: Fee::ZERO,
            one_for_zero_swap_fee: Fee::ZERO,
        }
    }

    /// Rebuilds a cache from a snapshot's LP fee, where `None` means dynamic.
    #[inline]
    pub(crate) fn from_lp_fee(
        lp_fee: Option<Fee>,
        protocol_fee: ProtocolFee,
    ) -> Result<Self, crate::Error> {
        let (zero_for_one_swap_fee, one_for_zero_swap_fee) = if let Some(lp_fee) = &lp_fee {
            calculate_effective_fee(lp_fee, &protocol_fee)?
        } else {
            (Fee::ZERO, Fee::ZERO)
        };

        Ok(Self {
            lp_fee,
            protocol_fee,
            zero_for_one_swap_fee,
            one_for_zero_swap_fee,
        })
    }

    /// Returns the pool LP fee before directional protocol fees are applied.
    ///
    /// `None` means a dynamic-fee pool has not had its fee resolved by a
    /// before-swap hook yet.
    #[inline]
    pub fn lp_fee(&self) -> &Option<Fee> {
        &self.lp_fee
    }

    /// Returns the directional protocol-fee configuration.
    #[inline]
    pub fn protocol_fee(&self) -> &ProtocolFee {
        &self.protocol_fee
    }

    /// Returns the cached effective fee for token0-to-token1 swaps.
    #[inline]
    pub fn zero_for_one_swap_fee(&self) -> &Fee {
        &self.zero_for_one_swap_fee
    }

    /// Returns the cached effective fee for the requested swap direction.
    ///
    /// Pass `true` for token0-to-token1 swaps and `false` for token1-to-token0
    /// swaps.
    #[inline]
    pub fn swap_fee(&self, zero_for_one: bool) -> &Fee {
        if zero_for_one {
            &self.zero_for_one_swap_fee
        } else {
            &self.one_for_zero_swap_fee
        }
    }

    /// Replaces the LP fee and refreshes both cached effective swap fees.
    ///
    /// No-ops if the LP fee is unchanged, including when this is the first LP
    /// fee set on a dynamic-fee pool constructed with `lp_fee: None`. Returns
    /// an error if the new LP fee produces an effective fee that exceeds the
    /// V4 swap-fee limit; on error, the cached state is left unchanged.
    #[inline]
    pub fn set_lp_fee(&mut self, lp_fee: Fee) -> Result<(), crate::Error> {
        if self.lp_fee.as_ref() == Some(&lp_fee) {
            return Ok(());
        }

        let (zero_for_one_swap_fee, one_for_zero_swap_fee) =
            calculate_effective_fee(&lp_fee, &self.protocol_fee)?;

        self.zero_for_one_swap_fee = zero_for_one_swap_fee;
        self.one_for_zero_swap_fee = one_for_zero_swap_fee;

        self.lp_fee = Some(lp_fee);

        Ok(())
    }

    /// Replaces protocol fees and refreshes both cached effective swap fees.
    ///
    /// No-ops if the protocol fee is unchanged. If no LP fee has been set yet
    /// (a dynamic-fee pool awaiting its first before-swap hook), the protocol
    /// fee is still stored but both cached effective fees stay [`Fee::ZERO`].
    /// Returns an error if the new protocol fees produce an effective fee
    /// that exceeds the V4 swap-fee limit; on error, the cached state is left
    /// unchanged.
    #[inline]
    pub fn set_protocol_fee(&mut self, protocol_fee: ProtocolFee) -> Result<(), crate::Error> {
        if self.protocol_fee == protocol_fee {
            return Ok(());
        }

        let (zero_for_one_swap_fee, one_for_zero_swap_fee) = match &self.lp_fee {
            Some(lp_fee) => calculate_effective_fee(lp_fee, &protocol_fee)?,
            None => (Fee::ZERO, Fee::ZERO),
        };

        self.zero_for_one_swap_fee = zero_for_one_swap_fee;
        self.one_for_zero_swap_fee = one_for_zero_swap_fee;

        self.protocol_fee = protocol_fee;

        Ok(())
    }
}

#[inline]
fn calculate_effective_fee(
    lp_fee: &Fee,
    protocol_fee: &ProtocolFee,
) -> Result<(Fee, Fee), crate::Error> {
    Ok((
        protocol_fee.calculate_swap_fee(true, lp_fee)?,
        protocol_fee.calculate_swap_fee(false, lp_fee)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fee(pips: u32) -> Fee {
        Fee::new(pips).unwrap()
    }

    fn new_swap_fee(lp_fee: Fee, protocol_fee: ProtocolFee) -> SwapFee {
        SwapFee::new(lp_fee, protocol_fee).unwrap()
    }

    #[test]
    fn new_caches_directional_effective_fees() {
        let protocol_fee = ProtocolFee::new(100, 500).unwrap();
        let swap_fee = new_swap_fee(fee(3_000), protocol_fee);

        assert_eq!(swap_fee.lp_fee(), &Some(fee(3_000)));
        assert_eq!(swap_fee.protocol_fee(), &protocol_fee);
        assert_eq!(swap_fee.zero_for_one_swap_fee().pips(), 3_100);
        assert_eq!(swap_fee.swap_fee(true).pips(), 3_100);
        assert_eq!(swap_fee.swap_fee(false).pips(), 3_499);
    }

    #[test]
    fn new_dynamic_caches_zero_until_the_lp_fee_is_resolved() {
        let swap_fee = SwapFee::new_dynamic(ProtocolFee::new(100, 500).unwrap());

        assert_eq!(swap_fee.lp_fee(), &None);
        assert_eq!(swap_fee.swap_fee(true), &Fee::ZERO);
        assert_eq!(swap_fee.swap_fee(false), &Fee::ZERO);
    }

    #[test]
    fn set_lp_fee_recalculates_both_directions() {
        let mut swap_fee = new_swap_fee(fee(3_000), ProtocolFee::new(100, 500).unwrap());

        swap_fee.set_lp_fee(fee(1_000)).unwrap();

        assert_eq!(swap_fee.lp_fee(), &Some(fee(1_000)));
        assert_eq!(swap_fee.swap_fee(true).pips(), 1_100);
        assert_eq!(swap_fee.swap_fee(false).pips(), 1_500);
    }

    #[test]
    fn set_lp_fee_resolves_the_first_fee_of_a_dynamic_fee_pool() {
        let mut swap_fee = SwapFee::new_dynamic(ProtocolFee::new(100, 500).unwrap());

        swap_fee.set_lp_fee(fee(3_000)).unwrap();

        assert_eq!(swap_fee.lp_fee(), &Some(fee(3_000)));
        assert_eq!(swap_fee.swap_fee(true).pips(), 3_100);
        assert_eq!(swap_fee.swap_fee(false).pips(), 3_499);
    }

    #[test]
    fn set_lp_fee_is_a_no_op_when_unchanged() {
        let mut swap_fee = new_swap_fee(fee(3_000), ProtocolFee::new(100, 500).unwrap());

        assert_eq!(swap_fee.set_lp_fee(fee(3_000)), Ok(()));
        assert_eq!(swap_fee.swap_fee(true).pips(), 3_100);
        assert_eq!(swap_fee.swap_fee(false).pips(), 3_499);
    }

    #[test]
    fn set_protocol_fee_recalculates_both_directions() {
        let mut swap_fee = new_swap_fee(fee(3_000), ProtocolFee::new(100, 500).unwrap());

        swap_fee
            .set_protocol_fee(ProtocolFee::new(0, 1_000).unwrap())
            .unwrap();

        assert_eq!(
            swap_fee.protocol_fee(),
            &ProtocolFee::new(0, 1_000).unwrap()
        );
        assert_eq!(swap_fee.swap_fee(true).pips(), 3_000);
        assert_eq!(swap_fee.swap_fee(false).pips(), 3_997);
    }

    #[test]
    fn set_protocol_fee_updates_config_but_keeps_zero_fees_without_an_lp_fee() {
        let mut swap_fee = SwapFee::new_dynamic(ProtocolFee::ZERO);
        let protocol_fee = ProtocolFee::new(100, 500).unwrap();

        swap_fee.set_protocol_fee(protocol_fee).unwrap();

        assert_eq!(swap_fee.protocol_fee(), &protocol_fee);
        assert_eq!(swap_fee.swap_fee(true), &Fee::ZERO);
        assert_eq!(swap_fee.swap_fee(false), &Fee::ZERO);
    }

    #[test]
    fn set_lp_fee_leaves_state_unchanged_on_failure() {
        let mut swap_fee = new_swap_fee(fee(3_000), ProtocolFee::new(100, 500).unwrap());

        assert_eq!(
            swap_fee.set_lp_fee(Fee::DYNAMIC),
            Err(crate::Error::InvalidLpFee)
        );

        assert_eq!(swap_fee.lp_fee(), &Some(fee(3_000)));
        assert_eq!(swap_fee.swap_fee(true).pips(), 3_100);
        assert_eq!(swap_fee.swap_fee(false).pips(), 3_499);
    }

    #[test]
    fn set_protocol_fee_is_a_no_op_when_unchanged() {
        let protocol_fee = ProtocolFee::new(100, 500).unwrap();
        let mut swap_fee = new_swap_fee(fee(3_000), protocol_fee);

        assert_eq!(swap_fee.set_protocol_fee(protocol_fee), Ok(()));
        assert_eq!(swap_fee.swap_fee(true).pips(), 3_100);
        assert_eq!(swap_fee.swap_fee(false).pips(), 3_499);
    }
}
