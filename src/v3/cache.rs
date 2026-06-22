use std::sync::Arc;

use dashmap::DashMap;
use ruint::aliases::U256;

use crate::v3::{
    error::SwapSimError,
    sqrt_price_math::{get_amount0_delta, get_amount1_delta},
    tick_math::get_sqrt_price_at_tick as compute_sqrt_price_at_tick,
};

const MAX_SWAP_FEE: u32 = 1_000_000;

/// Cached amount required to fully move from `sqrt_start_x96` to one tick boundary.
///
/// The map key is intentionally only `tick_idx`; correctness is protected by
/// validating the state-dependent fields stored inside this value before reuse.
#[derive(Debug, Clone)]
pub struct TickCrossCacheEntry {
    pub sqrt_start_x96: U256,
    pub sqrt_boundary_x96: U256,
    pub liquidity: u128,
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
        sqrt_start_x96: U256,
        sqrt_boundary_x96: U256,
        liquidity: u128,
        fee_pips: u32,
    ) -> bool {
        self.sqrt_start_x96 == sqrt_start_x96
            && self.sqrt_boundary_x96 == sqrt_boundary_x96
            && self.liquidity == liquidity
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
    sqrt_tick_cache: DashMap<i32, U256>,

    zero_for_one_cross: DashMap<i32, Arc<TickCrossCacheEntry>>,
    one_for_zero_cross: DashMap<i32, Arc<TickCrossCacheEntry>>,
}

impl PoolCache {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn get_sqrt_price_at_tick(&self, tick_idx: i32) -> Result<U256, SwapSimError> {
        if let Some(cached) = self.sqrt_tick_cache.get(&tick_idx) {
            return Ok(*cached.value());
        }

        let sqrt_price = compute_sqrt_price_at_tick(tick_idx)?;
        self.sqrt_tick_cache.insert(tick_idx, sqrt_price);

        Ok(sqrt_price)
    }

    #[inline(always)]
    fn cross_map(&self, zero_for_one: bool) -> &DashMap<i32, Arc<TickCrossCacheEntry>> {
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
        tick_idx: i32,
        zero_for_one: bool,
        sqrt_start_x96: U256,
        sqrt_boundary_x96: U256,
        liquidity: u128,
        fee_pips: u32,
    ) -> Result<Option<Arc<TickCrossCacheEntry>>, SwapSimError> {
        if liquidity == 0 {
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
        sqrt_start_x96: U256,
        sqrt_boundary_x96: U256,
        liquidity: u128,
        fee_pips: u32,
    ) -> Result<TickCrossCacheEntry, SwapSimError> {
        let liquidity_u = U256::from(liquidity);

        let amount_in_to_cross = if zero_for_one {
            get_amount0_delta(sqrt_boundary_x96, sqrt_start_x96, liquidity_u, true)
                .map_err(|_| SwapSimError::AmountOverflow)?
        } else {
            get_amount1_delta(sqrt_start_x96, sqrt_boundary_x96, liquidity_u, true)
                .map_err(|_| SwapSimError::AmountOverflow)?
        };

        let amount_out_to_cross = if zero_for_one {
            get_amount1_delta(sqrt_boundary_x96, sqrt_start_x96, liquidity_u, false)
                .map_err(|_| SwapSimError::AmountOverflow)?
        } else {
            get_amount0_delta(sqrt_start_x96, sqrt_boundary_x96, liquidity_u, false)
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

#[inline(always)]
fn mul_div_u32_ceil(a: U256, mul: u32, div: u32) -> Result<U256, SwapSimError> {
    let (q, r) = mul_div_u32_div_rem(a, mul, div)?;

    if r == 0 {
        Ok(q)
    } else {
        q.checked_add(U256::ONE).ok_or(SwapSimError::AmountOverflow)
    }
}

/// Exact U256 * u32 / u32 with remainder.
/// Avoids overflowing `a * mul`.
#[inline(always)]
fn mul_div_u32_div_rem(a: U256, mul: u32, div: u32) -> Result<(U256, u32), SwapSimError> {
    if div == 0 {
        return Err(SwapSimError::AmountOverflow);
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
        return Err(SwapSimError::AmountOverflow);
    }

    Ok((U256::from_limbs([q[0], q[1], q[2], q[3]]), rem as u32))
}
