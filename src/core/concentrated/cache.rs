use std::{ops::Deref, sync::Arc};

use dashmap::DashMap;
use ruint::aliases::U256;

use crate::{
    Error as SwapSimError,
    core::{
        math::{
            small_ratio::mul_div_u32_ceil,
            sqrt_price::{get_amount0_delta, get_amount1_delta},
            tick::get_sqrt_price_at_tick as compute_sqrt_price_at_tick,
        },
        types::{nonzero::NonZeroLiquidity, sqrt_price::SqrtPriceX96, tick::TickIndex},
    },
};

const MAX_SWAP_FEE: u32 = 1_000_000;

/// Cached amount required to fully move from `sqrt_start_x96` to one tick boundary.
///
/// The map key is intentionally only `tick_idx`; correctness is protected by
/// validating the state-dependent fields stored inside this value before reuse.
#[derive(Debug, Clone)]
pub struct TickCrossCacheEntry {
    pub sqrt_start_x96: SqrtPriceX96,
    pub sqrt_boundary_x96: SqrtPriceX96,
    pub liquidity: NonZeroLiquidity,
    pub fee_pips: u32,

    /// Fee-excluded input needed to reach the boundary.
    pub amount_in_to_cross: U256,

    /// Swap fee charged on `amount_in_to_cross`.
    pub fee_amount_to_cross: U256,

    /// Total exact-input amount consumed if the boundary is fully crossed.
    /// `gross_in_to_cross = amount_in_to_cross + fee_amount_to_cross`.
    pub gross_in_to_cross: U256,

    /// Output produced if the boundary is fully crossed.
    pub amount_out_to_cross: U256,
}

impl TickCrossCacheEntry {
    #[inline(always)]
    pub fn matches_state(
        &self,
        sqrt_start_x96: SqrtPriceX96,
        sqrt_boundary_x96: SqrtPriceX96,
        liquidity: NonZeroLiquidity,
        fee_pips: u32,
    ) -> bool {
        self.sqrt_start_x96 == sqrt_start_x96
            && self.sqrt_boundary_x96 == sqrt_boundary_x96
            && self.liquidity.deref() == liquidity.deref()
            && self.fee_pips == fee_pips
    }

    #[inline(always)]
    pub fn can_cross_exact_in(&self, amount_remaining: U256) -> bool {
        !self.gross_in_to_cross.is_zero() && amount_remaining >= self.gross_in_to_cross
    }

    #[inline(always)]
    pub fn can_cross_exact_out(&self, amount_remaining: U256) -> bool {
        !self.amount_out_to_cross.is_zero() && amount_remaining >= self.amount_out_to_cross
    }
}

/// Pool-local cache for immutable tick sqrt prices and state-dependent boundary-cross amounts.
///
/// `sqrt_tick_cache` is immutable by construction: tick -> sqrtPrice never changes.
/// `*_cross` entries are state-dependent and must be validated before reuse.
#[derive(Debug, Default)]
pub struct PoolCache {
    sqrt_tick_cache: DashMap<TickIndex, SqrtPriceX96>,

    zero_for_one_cross: DashMap<TickIndex, Arc<TickCrossCacheEntry>>,
    one_for_zero_cross: DashMap<TickIndex, Arc<TickCrossCacheEntry>>,
}

impl PoolCache {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn get_sqrt_price_at_tick(
        &self,
        tick_idx: TickIndex,
    ) -> Result<SqrtPriceX96, SwapSimError> {
        if let Some(cached) = self.sqrt_tick_cache.get(&tick_idx) {
            return Ok(*cached.value());
        }

        let sqrt_price = compute_sqrt_price_at_tick(tick_idx);
        self.sqrt_tick_cache.insert(tick_idx, sqrt_price);

        Ok(sqrt_price)
    }

    #[inline(always)]
    fn cross_map(&self, zero_for_one: bool) -> &DashMap<TickIndex, Arc<TickCrossCacheEntry>> {
        if zero_for_one {
            &self.zero_for_one_cross
        } else {
            &self.one_for_zero_cross
        }
    }

    /// Return a cached full-boundary-cross entry for the current state, or compute one.
    ///
    /// This function intentionally returns `Ok(None)` for cases that should fall back to the
    /// canonical swap-step path, such as zero liquidity, 100% fee, zero movement, or a boundary
    /// inconsistent with the swap direction.
    #[inline]
    pub fn get_or_compute_boundary_cross(
        &self,
        tick_idx: TickIndex,
        zero_for_one: bool,
        sqrt_start_x96: SqrtPriceX96,
        sqrt_boundary_x96: SqrtPriceX96,
        liquidity: NonZeroLiquidity,
        fee_pips: u32,
    ) -> Result<Option<Arc<TickCrossCacheEntry>>, SwapSimError> {
        if liquidity.is_zero() {
            return Ok(None);
        }

        // With 100% fee, exact-input has zero usable amount. Let the canonical path handle it.
        if fee_pips >= MAX_SWAP_FEE {
            return Ok(None);
        }

        // Direction guard. Equal start/boundary is no-progress and must not be cached.
        if zero_for_one {
            if sqrt_boundary_x96 >= sqrt_start_x96 {
                return Ok(None);
            }
        } else if sqrt_boundary_x96 <= sqrt_start_x96 {
            return Ok(None);
        }

        let map = self.cross_map(zero_for_one);

        if let Some(existing) = map.get(&tick_idx) {
            let entry = existing.value();

            if entry.matches_state(sqrt_start_x96, sqrt_boundary_x96, liquidity, fee_pips) {
                return Ok(Some(Arc::clone(entry)));
            }
        }

        let entry = Arc::new(Self::compute_boundary_cross_entry(
            zero_for_one,
            sqrt_start_x96,
            sqrt_boundary_x96,
            liquidity,
            fee_pips,
        )?);

        if entry.gross_in_to_cross.is_zero() || entry.amount_out_to_cross.is_zero() {
            return Ok(None);
        }

        map.insert(tick_idx, Arc::clone(&entry));
        Ok(Some(entry))
    }

    #[inline]
    fn compute_boundary_cross_entry(
        zero_for_one: bool,
        sqrt_start_x96: SqrtPriceX96,
        sqrt_boundary_x96: SqrtPriceX96,
        liquidity: NonZeroLiquidity,
        fee_pips: u32,
    ) -> Result<TickCrossCacheEntry, SwapSimError> {
        let (amount_in_to_cross, _) = if zero_for_one {
            get_amount0_delta(sqrt_boundary_x96, sqrt_start_x96, liquidity, true)
                .map_err(|_| SwapSimError::AmountOverflow)?
        } else {
            get_amount1_delta(sqrt_start_x96, sqrt_boundary_x96, liquidity, true)
                .map_err(|_| SwapSimError::AmountOverflow)?
        };

        let (amount_out_to_cross, _) = if zero_for_one {
            get_amount1_delta(sqrt_boundary_x96, sqrt_start_x96, liquidity, false)
                .map_err(|_| SwapSimError::AmountOverflow)?
        } else {
            get_amount0_delta(sqrt_start_x96, sqrt_boundary_x96, liquidity, false)
                .map_err(|_| SwapSimError::AmountOverflow)?
        };

        let fee_amount_to_cross = fee_on_exact_input_cached(amount_in_to_cross, fee_pips)?;
        let gross_in_to_cross = amount_in_to_cross
            .checked_add(fee_amount_to_cross)
            .ok_or(SwapSimError::AmountOverflow)?;

        Ok(TickCrossCacheEntry {
            sqrt_start_x96,
            sqrt_boundary_x96,
            liquidity,
            fee_pips,
            amount_in_to_cross,
            fee_amount_to_cross,
            gross_in_to_cross,
            amount_out_to_cross,
        })
    }

    /// Clear only state-dependent crossing amounts.
    ///
    /// Do not clear `sqrt_tick_cache`; tick -> sqrtPrice is immutable.
    #[inline]
    pub fn clear_cross_amounts(&self) {
        self.zero_for_one_cross.clear();
        self.one_for_zero_cross.clear();
    }

    #[inline]
    pub fn clear_all(&self) {
        self.sqrt_tick_cache.clear();
        self.clear_cross_amounts();
    }
}

#[inline(always)]
fn fee_on_exact_input_cached(amount_in: U256, fee_pips: u32) -> Result<U256, SwapSimError> {
    if amount_in.is_zero() || fee_pips == 0 {
        return Ok(U256::ZERO);
    }

    if fee_pips >= MAX_SWAP_FEE {
        return Err(SwapSimError::FeeTooLarge);
    }

    mul_div_u32_ceil(amount_in, fee_pips, MAX_SWAP_FEE - fee_pips)
}
