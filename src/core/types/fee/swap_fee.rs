use serde::{Deserialize, Serialize};

use crate::{core::types::fee::protocol_fee::ProtocolFee, v4::Fee};

/// Effective swap-fee configuration for a pool.
///
/// Uniswap V4 applies LP and protocol fees as one effective input fee, but the
/// protocol component can differ by swap direction. This type stores the raw
/// LP fee and protocol-fee configuration, then caches the calculated effective
/// fees for both token0-to-token1 and token1-to-token0 swaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapFee {
    lp_fee: Fee,
    protocol_fee: ProtocolFee,
    zero_for_one_swap_fee: Fee,
    one_for_zero_swap_fee: Fee,
}

impl SwapFee {
    /// Builds a swap-fee cache from the pool LP fee and protocol fees.
    ///
    /// Returns an error if either directional effective fee exceeds the V4
    /// swap-fee limit.
    #[inline]
    pub fn new(lp_fee: Fee, protocol_fee: ProtocolFee) -> Result<Self, crate::Error> {
        let zero_for_one_swap_fee = protocol_fee.calculate_swap_fee(true, &lp_fee)?;
        let one_for_zero_swap_fee = protocol_fee.calculate_swap_fee(false, &lp_fee)?;

        Ok(Self {
            lp_fee,
            protocol_fee,
            zero_for_one_swap_fee,
            one_for_zero_swap_fee,
        })
    }

    /// Returns the pool LP fee before directional protocol fees are applied.
    #[inline]
    pub fn lp_fee(&self) -> &Fee {
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

    /// Replaces protocol fees and refreshes both cached effective swap fees.
    ///
    /// Returns an error if the new protocol fees produce an effective fee that
    /// exceeds the V4 swap-fee limit.
    #[inline]
    pub fn set_protocol_fee(&mut self, protocol_fee: ProtocolFee) -> Result<(), crate::Error> {
        self.protocol_fee = protocol_fee;
        self.recalculate_swap_fee()
    }

    /// Recomputes the directional effective fees from the stored inputs.
    #[inline]
    fn recalculate_swap_fee(&mut self) -> Result<(), crate::Error> {
        self.zero_for_one_swap_fee = self.protocol_fee.calculate_swap_fee(true, &self.lp_fee)?;
        self.one_for_zero_swap_fee = self.protocol_fee.calculate_swap_fee(false, &self.lp_fee)?;

        Ok(())
    }
}
