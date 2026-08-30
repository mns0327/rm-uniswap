use alloy::primitives::Address;
use ruint::aliases::U256;

use crate::{
    core::types::pool_key::PoolKey,
    v4::{BalanceDelta, ModifyLiquidityParams, SwapParams},
};

/// Uniswap V4-style hook callbacks for concentrated-liquidity pools.
///
/// The real V4 PoolManager decides which callbacks to invoke from permission
/// bits encoded in the hook contract address. This trait models only the
/// callback surface: callers decide whether a hook is installed and when each
/// method should run.
///
/// `sender` is the original actor that started the pool operation, `pool`
/// identifies the pool being acted on, and `hook_data` is opaque caller-provided
/// data forwarded to the hook implementation.
pub trait Hooks: Send + Sync {
    /// Called before liquidity is added to a position.
    ///
    /// Implementations may validate the request or use `hook_data` to enforce
    /// application-specific policy before any pool or position state changes are
    /// committed.
    fn before_add_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called after liquidity has been added.
    ///
    /// `delta` is the caller-facing balance delta for the add-liquidity action.
    /// `fees_accrued` contains fees realized since the position was last
    /// updated or collected.
    fn after_add_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        delta: BalanceDelta,
        fees_accrued: BalanceDelta,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called before liquidity is removed from a position.
    ///
    /// Implementations may reject the request before pool or position state is
    /// mutated.
    fn before_remove_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called after liquidity has been removed.
    ///
    /// `delta` is the caller-facing balance delta for the remove-liquidity
    /// action. `fees_accrued` contains fees realized since the position was last
    /// updated or collected.
    fn after_remove_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        delta: BalanceDelta,
        fees_accrued: BalanceDelta,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called before a swap is executed.
    ///
    /// `params` uses V4 `amountSpecified` semantics: negative for exact input
    /// and positive for exact output.
    fn before_swap(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: SwapParams,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called after a swap has executed.
    ///
    /// `delta` is the caller / PoolManager balance delta produced by the swap:
    /// positive values are owed to the caller, and negative values are owed to
    /// the pool.
    fn after_swap(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: SwapParams,
        delta: BalanceDelta,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called before a donation is applied to the pool.
    ///
    /// `amount0` and `amount1` are the token amounts being donated to current
    /// in-range liquidity.
    fn before_donate(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        amount0: U256,
        amount1: U256,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called after a donation has been applied to the pool.
    ///
    /// `amount0` and `amount1` are the token amounts donated to current
    /// in-range liquidity.
    fn after_donate(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        amount0: U256,
        amount1: U256,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;
}
