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
        concentrated::{
            pool::{ModifyLiquidityParams, Pool as ConcentratedPool},
            position::{negative_amount, u256_to_i128},
        },
        types::delta::BalanceDelta,
    },
};
use alloy::primitives::{Address, B256};
use parking_lot::RwLock;

pub use super::position::PositionState;

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
    /// Principal delta. Positive means currencies owed to the pool.
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
    fn before_modify_liquidity(&self, _request: &ModifyRequest) -> Result<(), Error> {
        Ok(())
    }

    /// Return an optional hook delta to add to the manager-facing result.
    fn after_modify_liquidity(
        &self,
        _request: &ModifyRequest,
        _result: &ModifyResult,
    ) -> Result<BalanceDelta, Error> {
        Ok(BalanceDelta::default())
    }
}

#[derive(Clone)]
pub struct PositionManager {
    pool: ConcentratedPool,
    positions: Arc<RwLock<BTreeMap<u64, PositionInfo>>>,
    next_token_id: Arc<AtomicU64>,
    #[cfg(feature = "v4-hooks")]
    hook: Option<Arc<dyn LiquidityHook>>,
}

impl PositionManager {
    pub fn new(pool: ConcentratedPool) -> Self {
        Self {
            pool,
            positions: Arc::new(RwLock::new(BTreeMap::new())),
            next_token_id: Arc::new(AtomicU64::new(1)),
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

        match self.modify(
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
        self.modify(
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
        self.modify(
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
        self.modify(caller, token_id, 0, u128::MAX, u128::MAX, 0, 0)
    }

    pub fn burn(&self, caller: Address, token_id: u64) -> Result<(), Error> {
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
    fn modify(
        &self,
        caller: Address,
        token_id: u64,
        liquidity_delta: i128,
        amount0_max: u128,
        amount1_max: u128,
        amount0_min: u128,
        amount1_min: u128,
    ) -> Result<ModifyResult, Error> {
        let mut positions = self.positions.write();
        let snapshot = positions
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
        let predicted_fee0 = u256_to_i128(predicted_fees0)?;
        let predicted_fee1 = u256_to_i128(predicted_fees1)?;
        #[cfg(feature = "v4-hooks")]
        let predicted = ModifyResult {
            principal_delta: quoted,
            fee_delta: BalanceDelta {
                amount0: predicted_fee0,
                amount1: predicted_fee1,
            },
            liquidity: predicted_state.liquidity,
        };
        #[cfg(not(feature = "v4-hooks"))]
        let _ = (predicted_fee0, predicted_fee1);
        #[cfg(feature = "v4-hooks")]
        let hook_delta = if let Some(hook) = &self.hook {
            hook.after_modify_liquidity(&request, &predicted)?
        } else {
            BalanceDelta::default()
        };

        let pool_result = self.pool.modify_liquidity(ModifyLiquidityParams::new(
            snapshot.tick_lower,
            snapshot.tick_upper,
            liquidity_delta,
        ))?;

        let mut next_state = snapshot.state;
        let (fees0, fees1) = next_state.update(
            liquidity_delta,
            pool_result.fee_growth_inside0_x128,
            pool_result.fee_growth_inside1_x128,
        )?;
        let fee_delta = BalanceDelta {
            amount0: u256_to_i128(fees0)?,
            amount1: u256_to_i128(fees1)?,
        };
        #[allow(unused_mut)]
        let mut result = ModifyResult {
            principal_delta: pool_result.delta,
            fee_delta,
            liquidity: next_state.liquidity,
        };

        #[cfg(feature = "v4-hooks")]
        {
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
        }

        let position = positions
            .get_mut(&token_id)
            .ok_or(Error::PositionNotFound)?;
        position.state = next_state;
        Ok(result)
    }
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
        let amount0 = u128::try_from(delta.amount0).map_err(|_| Error::AmountOverflow)?;
        let amount1 = u128::try_from(delta.amount1).map_err(|_| Error::AmountOverflow)?;
        if amount0 > amount0_max || amount1 > amount1_max {
            return Err(Error::SlippageExceeded);
        }
    } else {
        let amount0 = negative_amount(delta.amount0)?;
        let amount1 = negative_amount(delta.amount1)?;
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
