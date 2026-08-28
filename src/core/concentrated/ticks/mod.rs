//! Tick storage adapters for concentrated-liquidity swap execution.
//!
//! The concrete [`TickSlab`] owns the initialized tick data, while
//! [`TickAccess`] gives the shared swap loop one interface for both read-only
//! quoting and mutating swaps. Read-only access can traverse boundaries and
//! compute liquidity deltas without touching fee-growth-outside state; mutable
//! access performs the full Uniswap tick-crossing update before returning the
//! same liquidity delta.

use ruint::aliases::U256;

use crate::{
    core::types::tick_slab_indexer::TickSlabIndexer,
    v4::{TickIndex, TickInfo},
    Error,
};

use slab::TickSlab;

#[allow(dead_code)]
pub(crate) mod bitmap;
// pub(crate) mod pool_ticks;
#[allow(dead_code)]
pub mod slab;
#[allow(dead_code)]
pub(crate) mod slab_value;

/// Minimal tick-store interface needed by the shared swap loop.
///
/// [`Pool::quote_swap`](crate::core::concentrated::pool::Pool::quote_swap) uses
/// the `&TickSlab` implementation to simulate crossings without changing
/// stored fee-growth-outside values. [`Pool::swap`](crate::core::concentrated::pool::Pool::swap)
/// uses the `&mut TickSlab` implementation so the same loop can commit each
/// crossed tick as it advances through initialized boundaries.
pub trait TickAccess {
    /// Builds this store's spacing-aware slab coordinate from a protocol tick.
    fn indexer(&self, tick_idx: TickIndex) -> TickSlabIndexer;

    /// Crosses an initialized tick and returns the active-liquidity delta.
    ///
    /// Implementations decide whether crossing is observational or mutating.
    /// Quote paths ignore the supplied fee-growth globals and return the stored
    /// `liquidity_net`; mutating paths use those globals to flip fee accounting
    /// around the crossed boundary before returning the same delta.
    fn cross_tick(
        &mut self,
        tick_indexer: TickSlabIndexer,
        fee_growth_outside0_x128: U256,
        fee_growth_outside1_x128: U256,
    ) -> Result<i128, Error>;

    /// Returns the next initialized boundary in swap direction.
    ///
    /// `zero_for_one` moves toward lower ticks, so implementations return the
    /// previous initialized boundary. The opposite direction moves toward
    /// higher ticks and returns the next initialized boundary.
    fn next_initialized_tick(
        &self,
        tick_indexer: TickSlabIndexer,
        zero_for_one: bool,
    ) -> Option<(TickSlabIndexer, &TickInfo)>;
}

/// Read-only tick access for quote simulations.
///
/// This implementation mirrors traversal and liquidity-delta behavior without
/// committing the fee-growth-outside flip that happens when a live swap crosses
/// a tick.
impl TickAccess for &TickSlab {
    #[inline(always)]
    fn indexer(&self, tick_idx: TickIndex) -> TickSlabIndexer {
        TickSlab::indexer(self, tick_idx)
    }

    #[inline(always)]
    fn cross_tick(
        &mut self,
        tick_indexer: TickSlabIndexer,
        _: U256,
        _: U256,
    ) -> Result<i128, Error> {
        (&**self)
            .get(tick_indexer)
            .map(|tick_info| tick_info.liquidity_net)
            .ok_or(Error::InvalidTick)
    }

    #[inline(always)]
    fn next_initialized_tick(
        &self,
        tick_indexer: TickSlabIndexer,
        zero_for_one: bool,
    ) -> Option<(TickSlabIndexer, &TickInfo)> {
        TickSlab::next_initialized_tick(self, tick_indexer, zero_for_one)
    }
}

/// Mutating tick access for live swaps.
///
/// Crossing delegates to [`TickSlab::cross_tick`], which updates
/// fee-growth-outside state and returns the boundary's liquidity delta for the
/// pool's active-liquidity update.
impl TickAccess for &mut TickSlab {
    #[inline(always)]
    fn indexer(&self, tick_idx: TickIndex) -> TickSlabIndexer {
        TickSlab::indexer(self, tick_idx)
    }

    #[inline(always)]
    fn cross_tick(
        &mut self,
        tick_indexer: TickSlabIndexer,
        fee_growth_outside0_x128: U256,
        fee_growth_outside1_x128: U256,
    ) -> Result<i128, Error> {
        TickSlab::cross_tick(
            self,
            tick_indexer,
            fee_growth_outside0_x128,
            fee_growth_outside1_x128,
        )
    }

    #[inline(always)]
    fn next_initialized_tick(
        &self,
        tick_indexer: TickSlabIndexer,
        zero_for_one: bool,
    ) -> Option<(TickSlabIndexer, &TickInfo)> {
        TickSlab::next_initialized_tick(self, tick_indexer, zero_for_one)
    }
}
