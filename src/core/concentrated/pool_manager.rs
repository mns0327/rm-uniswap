//! Single-pool manager facade for concentrated liquidity operations.

use std::ops::DerefMut;

use alloy::primitives::Address;
use ruint::aliases::U256;

use crate::{
    Error,
    core::{
        concentrated::{pool::Pool, position::Positions},
        types::{
            delta::{BalanceDelta, BeforeSwapDelta},
            hooks::{HookEnv, HooksImpl},
            params::{ModifyLiquidityParams, ModifyLiquidityResult, SwapParams, SwapParamsInner},
            pool_key::PoolKey,
        },
    },
};

/// Executes Uniswap V4-style pool operations against a single pool instance.
///
/// `SinglePoolManager` provides the operation lifecycle expected by hooks:
/// run the matching before-hook, apply the pool mutation, then run the
/// after-hook with the pool result. It is intentionally scoped to one
/// already-created pool and does not implement NFT-style position tokens,
/// multi-pool locking, or settlement accounting.
pub struct SinglePoolManager {
    pool: Pool<Positions>,
    hooks: Option<(PoolKey, Box<dyn HooksImpl>)>,
    block_number: u64,
}

impl SinglePoolManager {
    /// Creates a manager for an existing pool while resetting position storage.
    ///
    /// Pool configuration and price/tick state are preserved from `pool`; the
    /// manager owns a fresh [`Positions`] store for subsequent liquidity
    /// accounting.
    pub fn new(pool: Pool, hooks: Option<(PoolKey, Box<dyn HooksImpl>)>) -> Self {
        let Pool {
            state,
            swap_fee,
            tick_spacing,
            ticks,
            positions: _,
        } = pool;

        Self {
            pool: Pool {
                state,
                swap_fee,
                tick_spacing,
                ticks,
                positions: Positions::new(),
            },
            hooks,
            block_number: 0,
        }
    }

    /// Block number exposed to hooks through [`HookEnv`]. Defaults to `0`.
    pub fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Sets the block number subsequent operations execute in, for hooks that
    /// depend on it (e.g. per-block fee decay).
    pub fn set_block_number(&mut self, block_number: u64) {
        self.block_number = block_number;
    }

    /// Returns the managed pool for inspection.
    pub fn pool(&self) -> &Pool<Positions> {
        &self.pool
    }

    /// Returns the managed pool for direct low-level mutation.
    pub fn pool_mut(&mut self) -> &mut Pool<Positions> {
        &mut self.pool
    }

    /// Applies a liquidity update and returns the caller-facing accounting delta.
    ///
    /// The supplied `owner` and `salt` in [`ModifyLiquidityParams`] identify the
    /// position for pool-level accounting. Hook callbacks receive the same
    /// parameters used for the pool mutation, and any after-hook caller delta
    /// replaces the default pool-and-fee delta returned to the caller.
    pub fn modify_liquidity(
        &mut self,
        params: ModifyLiquidityParams,
        hook_data: &[u8],
    ) -> Result<ModifyLiquidityResult, Error> {
        if let Some((pool_key, hooks)) = self.hooks.as_mut() {
            hooks.before_modify_liquidity(&params.owner, pool_key, &params, hook_data)?;
        }

        let pool_result = self.pool.modify_liquidity(params)?;

        let caller_delta = pool_result.delta.checked_add(&pool_result.fee_delta)?;

        let caller_delta = match self.hooks.as_mut() {
            Some((pool_key, hooks)) => {
                let (caller_delta, _hook_delta) = hooks.after_modify_liquidity(
                    &params.owner,
                    pool_key,
                    &params,
                    caller_delta,
                    pool_result.fee_delta,
                    hook_data,
                )?;
                caller_delta
            }
            None => caller_delta,
        };

        Ok(ModifyLiquidityResult {
            delta: caller_delta,
            fee_delta: pool_result.fee_delta,
        })
    }

    /// Applies a swap and returns the delta that the caller should settle.
    ///
    /// A before-swap hook may change the executable swap amount and provide a
    /// pre-swap delta. The pool executes with that hook-adjusted amount, while
    /// the after-swap hook receives the original caller parameters, pool swap
    /// delta, and before-swap delta so it can produce the final caller delta.
    pub fn swap(
        &mut self,
        sender: Address,
        params: SwapParams,
        hook_data: &[u8],
    ) -> Result<BalanceDelta, Error> {
        if params.amount_specified.is_zero() {
            return Err(Error::ZeroValue);
        }

        let (amount_to_swap, before_swap_delta, lp_fee_override) = match self.hooks.as_mut() {
            Some((pool_key, hooks)) => {
                let env = HookEnv {
                    block_number: self.block_number,
                    pool_state: &self.pool.state,
                };
                let (amount_to_swap, before_swap_delta, lp_fee_override) = HooksImpl::before_swap(
                    hooks.deref_mut(),
                    &sender,
                    pool_key,
                    &params,
                    &env,
                    hook_data,
                )?;
                (amount_to_swap, before_swap_delta, lp_fee_override)
            }
            None => (params.amount_specified, BeforeSwapDelta::ZERO, None),
        };

        let executable_params = SwapParamsInner {
            amount_specified: amount_to_swap,
            lp_fee_override,
            zero_for_one: params.zero_for_one,
            sqrt_price_limit: params.sqrt_price_limit.clone(),
        };

        let swap_result = self.pool.swap(executable_params)?;

        match self.hooks.as_mut() {
            Some((pool_key, hooks)) => {
                let (caller_delta, _hook_delta) = HooksImpl::after_swap(
                    hooks.deref_mut(),
                    &sender,
                    pool_key,
                    params,
                    swap_result.swap_delta,
                    hook_data,
                    before_swap_delta,
                )?;

                Ok(caller_delta)
            }
            None => Ok(swap_result.swap_delta),
        }
    }

    /// Runs the donation hook lifecycle without mutating pool liquidity state.
    ///
    /// Donation accounting is intentionally deferred until the pool exposes a
    /// native donate operation. Until then this method preserves hook ordering
    /// and returns a zero caller delta.
    pub fn donate(
        &mut self,
        sender: Address,
        amount0: U256,
        amount1: U256,
        hook_data: &[u8],
    ) -> Result<BalanceDelta, Error> {
        if let Some((pool_key, hooks)) = self.hooks.as_mut() {
            hooks.before_donate(&sender, pool_key, amount0, amount1, hook_data)?;
            hooks.after_donate(&sender, pool_key, amount0, amount1, hook_data)?;
        }

        Ok(BalanceDelta::DEFAULT)
    }
}
