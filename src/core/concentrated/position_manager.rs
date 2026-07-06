//! Uniswap V4 position management and hook integration.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    Error,
    core::{
        concentrated::pool::{ModifyLiquidityParams, Pool as ConcentratedPool},
        types::delta::BalanceDelta,
    },
};
use alloy::primitives::{Address, B256};
use parking_lot::{Mutex, RwLock};
use ruint::aliases::U256;

pub use super::position::PositionState;

const I128_POSITIVE_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX >> 1, 0, 0]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionInfo {
    pub owner: Address,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub salt: B256,
    pub state: PositionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifyResult {
    /// Principal delta in caller / LP perspective.
    ///
    /// Negative means currencies owed to the pool; positive means currencies
    /// owed to the LP.
    pub principal_delta: BalanceDelta,
    /// Fees earned by the position. Positive means currencies owed to the LP.
    pub fee_delta: BalanceDelta,
    pub liquidity: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintResult {
    pub token_id: u64,
    pub result: ModifyResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifyRequest {
    pub token_id: u64,
    pub owner: Address,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity_delta: i128,
}

#[cfg(feature = "v4-hooks")]
pub trait LiquidityHook: Send + Sync {
    /// Called before quoting or mutating pool liquidity.
    ///
    /// The manager does not hold its position-map lock while invoking hooks.
    /// Hooks may inspect the manager through another shared handle, but should
    /// not recursively start another position mutation.
    fn before_modify_liquidity(&self, _request: &ModifyRequest) -> Result<(), Error> {
        Ok(())
    }

    /// Return an optional hook delta to add to the manager-facing result.
    ///
    /// This callback runs after the complete manager-facing result has been
    /// predicted, but before pool state is mutated. Hook errors and hook-delta
    /// overflow therefore leave both pool and position state unchanged.
    fn after_modify_liquidity(
        &self,
        _request: &ModifyRequest,
        _result: &ModifyResult,
    ) -> Result<BalanceDelta, Error> {
        Ok(BalanceDelta::default())
    }
}

/// Thread-safe position manager sharing one pool and position store.
///
/// Manager-owned mutations are serialized. User hooks run without the
/// position-map lock, so read-only inspection remains available while a hook
/// executes. Recursive position mutation from a hook is not supported.
#[derive(Clone)]
pub struct PositionManager {
    pool: ConcentratedPool,
    positions: Arc<RwLock<BTreeMap<u64, PositionInfo>>>,
    next_token_id: Arc<AtomicU64>,
    /// Serializes manager-owned mutations without holding the position map
    /// across user hooks or pool operations.
    mutation_lock: Arc<Mutex<()>>,
    #[cfg(feature = "v4-hooks")]
    hook: Option<Arc<dyn LiquidityHook>>,
}

impl PositionManager {
    pub fn new(pool: ConcentratedPool) -> Self {
        Self {
            pool,
            positions: Arc::new(RwLock::new(BTreeMap::new())),
            next_token_id: Arc::new(AtomicU64::new(1)),
            mutation_lock: Arc::new(Mutex::new(())),
            #[cfg(feature = "v4-hooks")]
            hook: None,
        }
    }

    #[cfg(feature = "v4-hooks")]
    pub fn with_hook(pool: ConcentratedPool, hook: Arc<dyn LiquidityHook>) -> Self {
        let mut manager = Self::new(pool);
        manager.hook = Some(hook);
        manager
    }

    pub fn pool(&self) -> &ConcentratedPool {
        &self.pool
    }

    pub fn position(&self, token_id: u64) -> Option<PositionInfo> {
        self.positions.read().get(&token_id).copied()
    }

    pub fn mint(
        &self,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        liquidity: u128,
        amount0_max: u128,
        amount1_max: u128,
    ) -> Result<MintResult, Error> {
        if liquidity == 0 || liquidity > i128::MAX as u128 {
            return Err(Error::ZeroLiquidity);
        }
        let _mutation_guard = self.mutation_lock.lock();
        let token_id = self.next_token_id.fetch_add(1, Ordering::Relaxed);
        let salt = token_id_salt(token_id);
        let info = PositionInfo {
            owner,
            tick_lower,
            tick_upper,
            salt,
            state: PositionState::default(),
        };
        self.positions.write().insert(token_id, info);

        match self.modify_locked(
            owner,
            token_id,
            liquidity as i128,
            amount0_max,
            amount1_max,
            0,
            0,
        ) {
            Ok(result) => Ok(MintResult { token_id, result }),
            Err(error) => {
                self.positions.write().remove(&token_id);
                Err(error)
            }
        }
    }

    pub fn increase_liquidity(
        &self,
        caller: Address,
        token_id: u64,
        liquidity: u128,
        amount0_max: u128,
        amount1_max: u128,
    ) -> Result<ModifyResult, Error> {
        if liquidity > i128::MAX as u128 {
            return Err(Error::LiquidityOverflow);
        }
        let _mutation_guard = self.mutation_lock.lock();
        self.modify_locked(
            caller,
            token_id,
            liquidity as i128,
            amount0_max,
            amount1_max,
            0,
            0,
        )
    }

    pub fn decrease_liquidity(
        &self,
        caller: Address,
        token_id: u64,
        liquidity: u128,
        amount0_min: u128,
        amount1_min: u128,
    ) -> Result<ModifyResult, Error> {
        if liquidity > i128::MAX as u128 {
            return Err(Error::LiquidityOverflow);
        }
        let _mutation_guard = self.mutation_lock.lock();
        self.modify_locked(
            caller,
            token_id,
            -(liquidity as i128),
            u128::MAX,
            u128::MAX,
            amount0_min,
            amount1_min,
        )
    }

    /// A zero-liquidity increase realizes fees without changing principal.
    pub fn collect_fees(&self, caller: Address, token_id: u64) -> Result<ModifyResult, Error> {
        let _mutation_guard = self.mutation_lock.lock();
        self.modify_locked(caller, token_id, 0, u128::MAX, u128::MAX, 0, 0)
    }

    pub fn burn(&self, caller: Address, token_id: u64) -> Result<(), Error> {
        let _mutation_guard = self.mutation_lock.lock();
        let mut positions = self.positions.write();
        let position = positions.get(&token_id).ok_or(Error::PositionNotFound)?;
        authorize(position.owner, caller)?;
        if position.state.liquidity != 0 {
            return Err(Error::PositionNotEmpty);
        }
        positions.remove(&token_id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn modify_locked(
        &self,
        caller: Address,
        token_id: u64,
        liquidity_delta: i128,
        amount0_max: u128,
        amount1_max: u128,
        amount0_min: u128,
        amount1_min: u128,
    ) -> Result<ModifyResult, Error> {
        let snapshot = self
            .positions
            .read()
            .get(&token_id)
            .copied()
            .ok_or(Error::PositionNotFound)?;
        authorize(snapshot.owner, caller)?;
        if liquidity_delta < 0 && liquidity_delta.unsigned_abs() > snapshot.state.liquidity {
            return Err(Error::LiquidityUnderflow);
        }

        #[cfg(feature = "v4-hooks")]
        let request = ModifyRequest {
            token_id,
            owner: snapshot.owner,
            tick_lower: snapshot.tick_lower,
            tick_upper: snapshot.tick_upper,
            liquidity_delta,
        };
        #[cfg(feature = "v4-hooks")]
        if let Some(hook) = &self.hook {
            hook.before_modify_liquidity(&request)?;
        }

        let quoted = self
            .pool
            .quote_modify_liquidity(ModifyLiquidityParams::new(
                snapshot.tick_lower,
                snapshot.tick_upper,
                liquidity_delta,
            ))?;
        validate_slippage(
            quoted,
            liquidity_delta,
            amount0_max,
            amount1_max,
            amount0_min,
            amount1_min,
        )?;

        let (growth0, growth1) = self
            .pool
            .fee_growth_inside(snapshot.tick_lower, snapshot.tick_upper)?;
        let mut predicted_state = snapshot.state;
        let (predicted_fees0, predicted_fees1) =
            predicted_state.update(liquidity_delta, growth0, growth1)?;
        let predicted_fee0 = u256_amount_to_i128(predicted_fees0)?;
        let predicted_fee1 = u256_amount_to_i128(predicted_fees1)?;
        #[cfg(feature = "v4-hooks")]
        let predicted = ModifyResult {
            principal_delta: quoted,
            fee_delta: BalanceDelta {
                amount0: predicted_fee0,
                amount1: predicted_fee1,
            },
            liquidity: predicted_state.liquidity,
        };
        #[cfg(feature = "v4-hooks")]
        let result = {
            let hook_delta = if let Some(hook) = &self.hook {
                hook.after_modify_liquidity(&request, &predicted)?
            } else {
                BalanceDelta::default()
            };

            apply_hook_delta(predicted, hook_delta)?
        };
        #[cfg(not(feature = "v4-hooks"))]
        let result = ModifyResult {
            principal_delta: quoted,
            fee_delta: BalanceDelta {
                amount0: predicted_fee0,
                amount1: predicted_fee1,
            },
            liquidity: predicted_state.liquidity,
        };

        let pool_result = self.pool.modify_liquidity(ModifyLiquidityParams::new(
            snapshot.tick_lower,
            snapshot.tick_upper,
            liquidity_delta,
        ))?;

        // All fallible manager-side calculations, including hooks and hook
        // delta overflow checks, completed before pool mutation. Committing the
        // predicted position state is now infallible.
        let result = ModifyResult {
            principal_delta: pool_result.delta,
            ..result
        };
        if snapshot.state.liquidity == 0 && liquidity_delta > 0 {
            predicted_state.fee_growth_inside0_last_x128 = pool_result.fee_growth_inside0_x128;
            predicted_state.fee_growth_inside1_last_x128 = pool_result.fee_growth_inside1_x128;
        }

        self.positions.write().insert(
            token_id,
            PositionInfo {
                state: predicted_state,
                ..snapshot
            },
        );
        Ok(result)
    }
}

#[cfg(feature = "v4-hooks")]
fn apply_hook_delta(
    mut result: ModifyResult,
    hook_delta: BalanceDelta,
) -> Result<ModifyResult, Error> {
    result.fee_delta.amount0 = result
        .fee_delta
        .amount0
        .checked_add(hook_delta.amount0)
        .ok_or(Error::AmountOverflow)?;
    result.fee_delta.amount1 = result
        .fee_delta
        .amount1
        .checked_add(hook_delta.amount1)
        .ok_or(Error::AmountOverflow)?;
    Ok(result)
}

fn validate_slippage(
    delta: BalanceDelta,
    liquidity_delta: i128,
    amount0_max: u128,
    amount1_max: u128,
    amount0_min: u128,
    amount1_min: u128,
) -> Result<(), Error> {
    if liquidity_delta >= 0 {
        let amount0 = negative_delta_to_u128(delta.amount0)?;
        let amount1 = negative_delta_to_u128(delta.amount1)?;
        if amount0 > amount0_max || amount1 > amount1_max {
            return Err(Error::SlippageExceeded);
        }
    } else {
        let amount0 = u128::try_from(delta.amount0).map_err(|_| Error::AmountOverflow)?;
        let amount1 = u128::try_from(delta.amount1).map_err(|_| Error::AmountOverflow)?;
        if amount0 < amount0_min || amount1 < amount1_min {
            return Err(Error::SlippageExceeded);
        }
    }
    Ok(())
}

fn authorize(owner: Address, caller: Address) -> Result<(), Error> {
    if owner == caller {
        Ok(())
    } else {
        Err(Error::Unauthorized)
    }
}

fn token_id_salt(token_id: u64) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[24..].copy_from_slice(&token_id.to_be_bytes());
    B256::from(bytes)
}

pub fn u256_amount_to_i128(value: U256) -> Result<i128, Error> {
    if value & !I128_POSITIVE_MASK != U256::ZERO {
        return Err(Error::AmountOverflow);
    }
    Ok(value.to::<u128>() as i128)
}

pub fn negative_delta_to_u128(value: i128) -> Result<u128, Error> {
    if value > 0 {
        return Err(Error::AmountOverflow);
    }
    Ok(value.unsigned_abs())
}
