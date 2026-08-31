//! Keyed position storage for concentrated-liquidity accounting.
//!
//! Positions mirror Uniswap v4's position-map shape: the key is the owner,
//! lower tick, upper tick, and salt, while the value stores liquidity plus the
//! last inside fee-growth checkpoints used to realize accrued fees.

use ahash::AHashMap;
use alloy::primitives::{Address, FixedBytes, U256};

use crate::v4::{Liquidity, TickIndex};

/// Map key for one Uniswap v4-style liquidity position.
///
/// The same owner may hold multiple positions over the same tick range by using
/// a distinct `salt`. Tick validation is owned by the caller; this key stores
/// already-validated [`TickIndex`] values exactly as supplied.
#[derive(Debug, Hash, PartialEq, Eq)]
pub struct PositionIndex {
    /// Account or manager address that owns the position.
    owner: Address,
    /// Inclusive lower tick of the position range.
    tick_lower: TickIndex,
    /// Exclusive upper tick of the position range.
    tick_upper: TickIndex,
    /// User-provided discriminator for otherwise identical owner/range pairs.
    salt: FixedBytes<32>,
}

/// Mutable accounting stored for one concentrated-liquidity position.
///
/// Fee-growth checkpoints are recorded when the position is last touched.
/// Comparing the current inside fee growth against these values yields the fees
/// accrued by the position's liquidity since that update.
#[derive(Debug)]
pub struct PositionState {
    /// Active liquidity owned by the position across its tick range.
    pub liquidity: Liquidity,
    /// Last token0 fee growth inside the position range, in X128 fixed point.
    pub fee_growth_inside0_last_x128: U256,
    /// Last token1 fee growth inside the position range, in X128 fixed point.
    pub fee_growth_inside1_last_x128: U256,
}

/// In-memory position map keyed by [`PositionIndex`].
///
/// The wrapper keeps the storage type explicit while allowing callers to depend
/// on [`PositionsAccess`] instead of a concrete hash-map implementation.
#[derive(Debug)]
pub struct Positions(pub AHashMap<PositionIndex, PositionState>);

/// Abstraction over optional position storage.
///
/// Implemented by [`Positions`] when per-position accounting is enabled and by
/// `()` when a caller wants a no-op store for quote-only or pool-only paths.
pub trait PositionsAccess {
    /// Whether this implementation actually persists position state.
    const ENABLE: bool;

    /// Returns the state for `index`, if present.
    fn get(&self, index: &PositionIndex) -> Option<&PositionState>;

    /// Returns mutable state for `index`, if present.
    fn get_mut(&mut self, index: &PositionIndex) -> Option<&mut PositionState>;

    /// Inserts or replaces the state for `index`, returning the previous value.
    fn insert(&mut self, index: PositionIndex, state: PositionState) -> Option<PositionState>;
}

impl Positions {
    /// Creates an empty position store.
    #[inline(always)]
    pub fn new() -> Self {
        Self(AHashMap::new())
    }

    /// Removes and returns the position state for `index`, if present.
    #[inline(always)]
    fn remove(&mut self, index: &PositionIndex) -> Option<PositionState> {
        self.0.remove(&index)
    }
}

impl PositionsAccess for Positions {
    /// Concrete stores retain position state.
    const ENABLE: bool = true;

    #[inline(always)]
    fn get(&self, index: &PositionIndex) -> Option<&PositionState> {
        self.0.get(index)
    }

    #[inline(always)]
    fn get_mut(&mut self, index: &PositionIndex) -> Option<&mut PositionState> {
        self.0.get_mut(index)
    }

    #[inline(always)]
    fn insert(&mut self, index: PositionIndex, state: PositionState) -> Option<PositionState> {
        self.0.insert(index, state)
    }
}

impl Default for Positions {
    #[inline(always)]
    fn default() -> Self {
        Self::new()
    }
}

impl PositionsAccess for () {
    /// The unit implementation intentionally drops all position state.
    const ENABLE: bool = false;

    #[inline(always)]
    fn get(&self, _: &PositionIndex) -> Option<&PositionState> {
        None
    }

    #[inline(always)]
    fn get_mut(&mut self, _: &PositionIndex) -> Option<&mut PositionState> {
        None
    }

    #[inline(always)]
    fn insert(&mut self, _: PositionIndex, _: PositionState) -> Option<PositionState> {
        None
    }
}
