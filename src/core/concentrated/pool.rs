//! Uniswap V4 pool simulator with full tick-crossing support.
//!
//! The swap loop follows V4 `Pool.sol`: it advances through initialized ticks,
//! updates liquidity on crossings, and returns deltas from the caller /
//! PoolManager accounting perspective. Negative delta values mean the caller
//! owes that token; positive values mean the caller receives it.
//!
//! `SwapParams::amount_specified` uses V4 `amountSpecified` semantics:
//! negative for exact input and positive for exact output. Tick spacing is
//! supplied from the `PoolKey` and validated together with pool snapshots and
//! tick stores.
//!
//! Read-only quote methods leave pool storage unchanged. [`Pool::swap`] runs
//! the same math through mutable state and tick access, so final price, tick,
//! liquidity, fee growth, and crossed tick fee state are reflected in pool
//! storage.

use alloy::primitives::I256;
use ruint::aliases::U256;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error as SwapSimError;
use crate::core::concentrated::position::{PositionIndex, PositionsAccess};
use crate::core::concentrated::ticks::TickAccess;
use crate::core::concentrated::ticks::slab::TickSlab;
use crate::core::types::fee::Fee;
use crate::core::types::fee::protocol_fee::{PIPS_DENOMINATOR, ProtocolFee};
use crate::core::types::fee::swap_fee::SwapFee;
use crate::core::types::liquidity::Liquidity;
use crate::core::types::nonzero::NonZeroLiquidity;
use crate::core::types::params::{
    ModifyLiquidityParams, ModifyLiquidityResult, StepComputations, SwapParams, SwapResult,
};
use crate::core::types::pool_state::{PoolState, StateAccess};
use crate::core::types::sqrt_price::SqrtPriceX96;
use crate::core::types::tick::TickIndex;
use crate::core::types::tick_spacing::TickSpacing;
use crate::core::{
    math::{swap::get_sqrt_price_target, tick::get_tick_at_sqrt_price},
    types::{PoolTicksSnapshot, TickInfo, delta::BalanceDelta},
};
use crate::v4::swap_math::compute_swap_step;

/// Complete V4 pool state required for a full tick-crossing simulation.
///
/// # Invariants
///
/// * `tick == floor(log_√1.0001(sqrt_price_x96))`. The simulator does not
///   re-derive tick from price; a mismatch produces wrong output.
/// * `swap_fee` must contain protocol-valid directional effective fees.
/// * `tick_spacing` is positive and protocol-valid — enforced by [`TickSpacing`].
/// * All `tick_idx` values in `ticks` must be within `[MIN_TICK, MAX_TICK]` and
///   be multiples of `tick_spacing`.
/// * When position storage is enabled, position liquidity must reconcile with
///   initialized tick liquidity.
#[derive(Debug)]
pub struct Pool<P = ()> {
    /// Current pool price, tick, liquidity, and fee-growth globals.
    pub state: PoolState,

    /// LP fee plus any directional protocol-fee configuration.
    pub swap_fee: SwapFee,

    /// Tick spacing from `PoolKey.tickSpacing`.
    pub tick_spacing: TickSpacing,

    /// Initialized ticks stored in this pool's spacing-aware slab index.
    pub ticks: TickSlab,

    /// Optional position store used for fee-growth checkpoint accounting.
    pub positions: P,
}

impl<P: Serialize + DeserializeOwned + Clone + PositionsAccess> Serialize for Pool<P> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.snapshot().serialize(serializer)
    }
}

impl<'de, P: Default + Clone + Serialize + DeserializeOwned + PositionsAccess> Deserialize<'de>
    for Pool<P>
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let snapshot = PoolSnapshot::deserialize(deserializer)?;
        Self::from_snapshot(snapshot).map_err(serde::de::Error::custom)
    }
}

impl<P: PositionsAccess> PoolSnapshot<P> {
    /// Validate all state that can affect swap correctness.
    pub fn validate(&self) -> Result<(), SwapSimError> {
        if self.ticks.tick_spacing != self.tick_spacing {
            return Err(SwapSimError::InvalidTickSpacing);
        }

        if !self.protocol_fee.is_valid() {
            return Err(SwapSimError::FeeTooLarge);
        }

        if get_tick_at_sqrt_price(&self.state.sqrt_price_x96) != self.state.tick {
            return Err(SwapSimError::InvalidTick);
        }

        let max_liquidity_per_tick = self.tick_spacing.max_liquidity_per_tick();
        let mut active_liquidity = 0i128;

        for (&tick_idx, info) in &self.ticks.inner {
            validate_tick_index_for_spacing(tick_idx, self.tick_spacing)?;

            if info.liquidity_gross.is_zero()
                || info.liquidity_net.unsigned_abs() > info.liquidity_gross.value()
            {
                return Err(SwapSimError::InvalidTick);
            }

            if info.liquidity_gross > max_liquidity_per_tick {
                return Err(SwapSimError::LiquidityOverflow);
            }

            if tick_idx <= self.state.tick {
                active_liquidity = active_liquidity
                    .checked_add(info.liquidity_net)
                    .ok_or(SwapSimError::LiquidityOverflow)?;
            }
        }

        if active_liquidity.is_negative()
            || active_liquidity.unsigned_abs() != *self.state.liquidity
        {
            return Err(SwapSimError::InvalidTick);
        }

        self.positions.validate_against_ticks(&self.ticks)
    }
}

/// Owned, serializable pool snapshot.
///
/// A snapshot owns plain pool, tick, and position data with no shared mutable
/// storage from the source [`Pool`]. Treat values decoded from external data as
/// untrusted and construct a runtime pool with [`Pool::try_from`] so all
/// invariants are validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "P: Serialize",
    deserialize = "P: Deserialize<'de> + Default"
))]
pub struct PoolSnapshot<P = ()> {
    /// Current pool price, active tick, liquidity, and fee-growth globals.
    pub state: PoolState,
    /// Base LP fee before protocol-fee adjustments.
    pub fee: Fee,
    /// Directional protocol-fee configuration.
    #[serde(default)]
    pub protocol_fee: ProtocolFee,
    /// Pool tick spacing used to validate position boundaries and tick storage.
    pub tick_spacing: TickSpacing,
    /// Serializable initialized tick storage.
    pub ticks: PoolTicksSnapshot,
    /// Serializable position storage, or `()` when positions are disabled.
    #[serde(default)]
    pub positions: P,
}

impl<P: Serialize + DeserializeOwned + Clone + PositionsAccess> Pool<P> {
    /// Capture an owned snapshot from the current pool storage.
    ///
    /// Pool state is copied by value, while ticks and positions are cloned into
    /// independent owned storage.
    pub fn snapshot(&self) -> PoolSnapshot<P> {
        PoolSnapshot {
            state: self.state.clone(),
            fee: *self.swap_fee.lp_fee(),
            protocol_fee: *self.swap_fee.protocol_fee(),
            tick_spacing: self.tick_spacing,
            ticks: PoolTicksSnapshot {
                tick_spacing: self.ticks.tick_spacing(),
                inner: self.ticks.snapshot(),
            },
            positions: self.positions.clone(),
        }
    }

    /// Build a live pool from a serializable snapshot after validation.
    ///
    /// This reconstructs tick storage and fee caches from owned data, returning
    /// an error if any pool, tick, fee, or position invariant is inconsistent.
    pub fn from_snapshot(snapshot: PoolSnapshot<P>) -> Result<Self, crate::Error> {
        snapshot.validate()?;

        let ticks = TickSlab::from_snapshot(snapshot.ticks.tick_spacing, snapshot.ticks.inner)?;
        let swap_fee = SwapFee::new(snapshot.fee, snapshot.protocol_fee)?;

        Ok(Self {
            state: snapshot.state,
            swap_fee,
            tick_spacing: snapshot.tick_spacing,
            ticks,
            positions: snapshot.positions,
        })
    }

    /// Construct a pool after validating every state invariant.
    ///
    /// Use this for external or dynamically assembled state. The legacy
    /// [`Pool::new`] constructor remains available for trusted, already
    /// validated state with empty tick storage.
    pub fn try_new(
        sqrt_price_x96: SqrtPriceX96,
        tick: TickIndex,
        liquidity: Liquidity,
        fee: Fee,
        tick_spacing: TickSpacing,
        ticks: TickSlab,
    ) -> Result<Self, SwapSimError> {
        Self::from_snapshot(PoolSnapshot {
            state: PoolState {
                sqrt_price_x96,
                tick,
                liquidity,
                fee_growth_global0_x128: U256::ZERO,
                fee_growth_global1_x128: U256::ZERO,
            },
            fee,
            protocol_fee: ProtocolFee::ZERO,
            tick_spacing,
            ticks: ticks.owned_snapshot(),
            positions: P::default(),
        })
    }

    /// Create an independent pool from the current validated snapshot.
    ///
    /// The fork owns its own state, tick storage, and position storage, so
    /// subsequent mutations cannot affect the source pool.
    pub fn try_fork(&self) -> Result<Self, SwapSimError> {
        Self::from_snapshot(self.snapshot())
    }
}

impl<P: Serialize + DeserializeOwned + Clone + PositionsAccess> TryFrom<PoolSnapshot<P>>
    for Pool<P>
{
    type Error = SwapSimError;

    fn try_from(snapshot: PoolSnapshot<P>) -> Result<Self, Self::Error> {
        Self::from_snapshot(snapshot)
    }
}

/// Tick-crossing audit payload for callers that record swap traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickCrossInfo {
    /// Input consumed up to and including this crossing.
    pub cumulative_input: U256,
    /// Initialized tick boundary that was crossed.
    pub tick: TickIndex,
    /// Active liquidity immediately after applying the crossing delta.
    pub liquidity_after: Liquidity,
    /// Token0 global fee-growth value used for the crossing update.
    pub fee_growth_global0_x128: U256,
    /// Token1 global fee-growth value used for the crossing update.
    pub fee_growth_global1_x128: U256,
}

impl<P: Default> Pool<P> {
    /// Construct a pool from trusted state without eager invariant validation.
    ///
    /// Prefer [`Pool::try_new`] for external input.
    pub fn new(
        sqrt_price_x96: SqrtPriceX96,
        tick: TickIndex,
        liquidity: Liquidity,
        swap_fee: SwapFee,
        tick_spacing: TickSpacing,
    ) -> Self {
        Pool {
            state: PoolState {
                sqrt_price_x96,
                tick,
                liquidity,
                fee_growth_global0_x128: U256::ZERO,
                fee_growth_global1_x128: U256::ZERO,
            },
            swap_fee,
            tick_spacing,
            ticks: TickSlab::new(tick_spacing),
            positions: P::default(),
        }
    }

    /// Replace protocol fees and refresh cached effective swap fees.
    #[inline(always)]
    pub fn set_protocol_fee(&mut self, protocol_fee: ProtocolFee) -> Result<(), crate::Error> {
        self.swap_fee.set_protocol_fee(protocol_fee)
    }
}

impl<P: PositionsAccess> Pool<P> {
    /// Apply a liquidity change and commit it to pool, tick, and position state.
    ///
    /// Implements the pool-facing portion of Uniswap V4's `modifyLiquidity`:
    ///
    /// 1. Validate `tick_lower < tick_upper` and tick spacing alignment.
    /// 2. Update lower/upper initialized ticks with the signed `liquidity_net`
    ///    convention, removing a boundary when its gross liquidity reaches zero.
    /// 3. Enforce `TickSpacing::max_liquidity_per_tick` when adding liquidity.
    /// 4. Realize position fees when position storage is enabled and `info` is
    ///    supplied.
    /// 5. Update active liquidity when the current tick is inside the range.
    /// 6. Return the caller / PoolManager principal delta for the liquidity
    ///    change plus any realized fee delta.
    ///
    /// A zero-liquidity update can realize position fees without changing pool
    /// principal or initialized ticks.
    pub fn modify_liquidity(
        &mut self,
        params: ModifyLiquidityParams,
    ) -> Result<ModifyLiquidityResult, SwapSimError> {
        let ModifyLiquidityParams {
            tick_lower,
            tick_upper,
            liquidity_delta,
            owner,
            salt,
        } = params;

        check_ticks(tick_lower, tick_upper, self.tick_spacing)?;

        let mut lower_sqrt_price: SqrtPriceX96 = SqrtPriceX96::MIN;
        let mut upper_sqrt_price: SqrtPriceX96 = SqrtPriceX96::MIN;

        let lower_fee_growth_outside0_x128;
        let lower_fee_growth_outside1_x128;
        let upper_fee_growth_outside0_x128;
        let upper_fee_growth_outside1_x128;

        if liquidity_delta != 0 {
            (
                lower_sqrt_price,
                lower_fee_growth_outside0_x128,
                lower_fee_growth_outside1_x128,
            ) = self.update_tick(tick_lower, liquidity_delta, false)?;
            (
                upper_sqrt_price,
                upper_fee_growth_outside0_x128,
                upper_fee_growth_outside1_x128,
            ) = self.update_tick(tick_upper, liquidity_delta, true)?;
        } else {
            (
                lower_fee_growth_outside0_x128,
                lower_fee_growth_outside1_x128,
            ) = self
                .ticks
                .get(self.ticks.indexer(tick_lower))
                .map(|tick| (tick.fee_growth_outside0_x128, tick.fee_growth_outside1_x128))
                .unwrap_or((U256::ZERO, U256::ZERO));
            (
                upper_fee_growth_outside0_x128,
                upper_fee_growth_outside1_x128,
            ) = self
                .ticks
                .get(self.ticks.indexer(tick_upper))
                .map(|tick| (tick.fee_growth_outside0_x128, tick.fee_growth_outside1_x128))
                .unwrap_or((U256::ZERO, U256::ZERO));
        }

        let fee_delta = {
            if P::ENABLE {
                let (fee_growth_inside0_x128, fee_growth_inside1_x128) = self.fee_growth_inside(
                    tick_lower,
                    tick_upper,
                    lower_fee_growth_outside0_x128,
                    lower_fee_growth_outside1_x128,
                    upper_fee_growth_outside0_x128,
                    upper_fee_growth_outside1_x128,
                )?;

                let position_idx = PositionIndex {
                    owner: owner,
                    tick_lower: tick_lower,
                    tick_upper: tick_upper,
                    salt: salt,
                };

                self.positions.update_position(position_idx, |position| {
                    match position.update(
                        liquidity_delta,
                        fee_growth_inside0_x128,
                        fee_growth_inside1_x128,
                    ) {
                        Ok((fees_owed0, fees_owed1)) => {
                            BalanceDelta::from_u256(fees_owed0, fees_owed1)
                        }
                        Err(e) => Err(e),
                    }
                })?
            } else {
                BalanceDelta::DEFAULT
            }
        };

        let delta = if liquidity_delta != 0 {
            if tick_lower <= self.state.tick && self.state.tick < tick_upper {
                self.state.liquidity = Liquidity::new(
                    self.state
                        .liquidity
                        .value()
                        .checked_add_signed(liquidity_delta)
                        .ok_or(SwapSimError::LiquidityOverflow)?,
                );
            }

            BalanceDelta::from_liquidity_principal(
                self.state.tick,
                tick_lower,
                tick_upper,
                self.state.sqrt_price_x96,
                lower_sqrt_price,
                upper_sqrt_price,
                liquidity_delta,
            )?
        } else {
            BalanceDelta::DEFAULT
        };

        Ok(ModifyLiquidityResult { delta, fee_delta })
    }

    /// Return the principal token delta for a liquidity change without mutation.
    ///
    /// This quote validates the same tick range as [`Pool::modify_liquidity`]
    /// but does not initialize ticks, update active liquidity, or realize
    /// position fees.
    pub fn quote_modify_liquidity(
        &self,
        params: ModifyLiquidityParams,
    ) -> Result<BalanceDelta, SwapSimError> {
        check_ticks(params.tick_lower, params.tick_upper, self.tick_spacing)?;
        BalanceDelta::from_liquidity_principal(
            self.state.tick,
            params.tick_lower,
            params.tick_upper,
            self.state.sqrt_price_x96,
            params.tick_lower.sqrt_price_x96(),
            params.tick_upper.sqrt_price_x96(),
            params.liquidity_delta,
        )
    }

    /// Return current all-time fee growth inside a tick range.
    ///
    /// The calculation follows Uniswap's inside/outside accumulator semantics
    /// with wrapping `U256` arithmetic. Missing boundary ticks contribute zero,
    /// which is useful for quotes over not-yet-initialized ranges.
    pub fn fee_growth_inside(
        &self,
        tick_lower: TickIndex,
        tick_upper: TickIndex,
        tick_lower_fee_growth_outside0_x128: U256,
        tick_lower_fee_growth_outside1_x128: U256,
        tick_upper_fee_growth_outside0_x128: U256,
        tick_upper_fee_growth_outside1_x128: U256,
    ) -> Result<(U256, U256), SwapSimError> {
        Ok(if self.state.tick < tick_lower {
            (
                tick_lower_fee_growth_outside0_x128
                    .wrapping_sub(tick_upper_fee_growth_outside0_x128),
                tick_lower_fee_growth_outside1_x128
                    .wrapping_sub(tick_upper_fee_growth_outside1_x128),
            )
        } else if self.state.tick >= tick_upper {
            (
                tick_upper_fee_growth_outside0_x128
                    .wrapping_sub(tick_lower_fee_growth_outside0_x128),
                tick_upper_fee_growth_outside1_x128
                    .wrapping_sub(tick_lower_fee_growth_outside1_x128),
            )
        } else {
            (
                self.state
                    .fee_growth_global0_x128
                    .wrapping_sub(tick_lower_fee_growth_outside0_x128)
                    .wrapping_sub(tick_upper_fee_growth_outside0_x128),
                self.state
                    .fee_growth_global1_x128
                    .wrapping_sub(tick_lower_fee_growth_outside1_x128)
                    .wrapping_sub(tick_upper_fee_growth_outside1_x128),
            )
        })
    }

    fn update_tick(
        &mut self,
        tick_index: TickIndex,
        liquidity_delta: i128,
        upper: bool,
    ) -> Result<(SqrtPriceX96, U256, U256), SwapSimError> {
        let tick_indexer = self.ticks.indexer(tick_index);

        let (
            liquidity_gross_before,
            liquidity_net_before,
            sqrt_price_x96,
            fee_growth_outside0_x128,
            fee_growth_outside1_x128,
        ) = if let Some(tick_info) = self.ticks.get(tick_indexer) {
            (
                tick_info.liquidity_gross,
                tick_info.liquidity_net,
                tick_info.sqrt_price_x96(),
                tick_info.fee_growth_outside0_x128,
                tick_info.fee_growth_outside1_x128,
            )
        } else {
            let sqrt_price = tick_index.sqrt_price_x96();
            self.ticks
                .insert(tick_indexer, TickInfo::new(tick_index, sqrt_price))?;
            (Liquidity::ZERO, 0, sqrt_price, U256::ZERO, U256::ZERO)
        };

        let liquidity_gross_after = Liquidity::new(
            liquidity_gross_before
                .checked_add_signed(liquidity_delta)
                .ok_or(SwapSimError::I128Overflow)?,
        );

        if liquidity_delta > 0 && liquidity_gross_after > self.ticks.max_liquidity_per_tick() {
            return Err(SwapSimError::LiquidityOverflow);
        }

        let liquidity_net = if upper {
            liquidity_net_before
                .checked_sub(liquidity_delta)
                .ok_or(SwapSimError::I128Overflow)?
        } else {
            liquidity_net_before
                .checked_add(liquidity_delta)
                .ok_or(SwapSimError::I128Overflow)?
        };

        if liquidity_gross_after.is_zero() {
            self.ticks.remove(tick_indexer);
            return Ok((
                sqrt_price_x96,
                fee_growth_outside0_x128,
                fee_growth_outside1_x128,
            ));
        }

        self.ticks
            .update_initialized_tick(tick_indexer, |tick_info| {
                // Newly initialized ticks on or below the current tick start
                // with outside fee growth set to the current global values.
                if liquidity_gross_before.is_zero() && tick_index <= self.state.tick {
                    tick_info.fee_growth_outside0_x128 = self.state.fee_growth_global0_x128;
                    tick_info.fee_growth_outside1_x128 = self.state.fee_growth_global1_x128;
                }

                tick_info.liquidity_gross = liquidity_gross_after;
                tick_info.liquidity_net = liquidity_net;

                Ok((
                    tick_info.sqrt_price_x96(),
                    tick_info.fee_growth_outside0_x128,
                    tick_info.fee_growth_outside1_x128,
                ))
            })
    }

    /// Execute a swap and commit the resulting pool and tick state.
    ///
    /// The returned deltas use the caller / PoolManager perspective: negative
    /// means the caller owes the token, positive means the caller receives it.
    pub fn swap(&mut self, params: SwapParams) -> Result<SwapResult, SwapSimError> {
        let result = { swap_inner(&mut self.state, &mut self.ticks, &self.swap_fee, params)? };

        Ok(result)
    }

    /// Simulate a swap without mutating pool or tick storage.
    ///
    /// The same swap loop is used as [`Pool::swap`], but read-only state and
    /// tick access ignore final state writes and tick-crossing fee-growth
    /// updates.
    pub fn quote_swap(&self, params: SwapParams) -> Result<SwapResult, SwapSimError> {
        swap_inner(&self.state, &self.ticks, &self.swap_fee, params)
    }
}

/// Run the shared V4 swap loop over pool-state and tick-store access.
///
/// Mutable access commits final pool state and tick-crossing fee-growth updates.
/// Read-only access executes the same traversal while ignoring those writes,
/// which keeps [`Pool::swap`] and [`Pool::quote_swap`] behavior aligned.
fn swap_inner<T: TickAccess, S: StateAccess>(
    mut state: S,
    mut ticks: T,
    swap_fee: &SwapFee,
    params: SwapParams,
) -> Result<SwapResult, SwapSimError> {
    let SwapParams {
        zero_for_one,
        amount_specified,
        sqrt_price_limit_x96,
    } = params;

    let exact_in = amount_specified.is_negative();

    let pool_state = state.get();

    let mut result = SwapResult {
        swap_delta: BalanceDelta::DEFAULT,
        amount_to_protocol: U256::ZERO,
        swap_fee: *swap_fee.swap_fee(zero_for_one),
        sqrt_price_x96: pool_state.sqrt_price_x96,
        tick: pool_state.tick,
        liquidity: pool_state.liquidity,
    };

    if amount_specified.is_zero() {
        return Ok(result);
    }

    if !pool_state.sqrt_price_x96.is_valid() {
        return Err(SwapSimError::InvalidPoolSqrtPrice);
    }

    // Validate the price limit against the current pool price.
    if zero_for_one {
        // V4: limit must be strictly below current price.
        if sqrt_price_limit_x96 >= pool_state.sqrt_price_x96 {
            return Err(SwapSimError::PriceLimitAlreadyExceeded);
        }
    } else {
        // V4: limit must be strictly above current price.
        if sqrt_price_limit_x96 <= pool_state.sqrt_price_x96 {
            return Err(SwapSimError::PriceLimitAlreadyExceeded);
        }
    }

    let mut amount_calculated = I256::ZERO;

    let mut step = StepComputations::DEFAULT;
    step.fee_growth_global_x128 = if zero_for_one {
        pool_state.fee_growth_global0_x128
    } else {
        pool_state.fee_growth_global1_x128
    };

    let mut amount_specified_remaining = amount_specified;

    while !(amount_specified_remaining.is_zero() || result.sqrt_price_x96 == sqrt_price_limit_x96) {
        if result.liquidity.is_zero() {
            break;
        }

        step.sqrt_price_start_x96 = result.sqrt_price_x96;

        if let Some(tick_info) =
            ticks.next_initialized_tick(ticks.indexer(result.tick), zero_for_one)
        {
            step.next_tick_indexer = tick_info.0;
            step.next_tick_info = *tick_info.1;
        } else if zero_for_one {
            step.next_tick_info.sqrt_price_x96 = SqrtPriceX96::MIN;
        } else {
            step.next_tick_info.sqrt_price_x96 = SqrtPriceX96::MAX;
        };

        step.sqrt_price_next_x96 = get_sqrt_price_target(
            zero_for_one,
            step.next_tick_info.sqrt_price_x96,
            sqrt_price_limit_x96,
        );

        let swap_step = compute_swap_step(
            result.sqrt_price_x96,
            step.sqrt_price_next_x96,
            unsafe { NonZeroLiquidity::new_unchecked(result.liquidity) },
            amount_specified_remaining,
            result.swap_fee,
        )?;

        result.sqrt_price_x96 = swap_step.sqrt_ratio_next_x96;
        step.amount_in = swap_step.amount_in;
        step.amount_out = swap_step.amount_out;
        step.fee_amount = swap_step.fee_amount;

        if amount_specified.is_positive() {
            amount_specified_remaining = amount_specified_remaining
                .checked_sub(
                    I256::try_from(swap_step.amount_out)
                        .map_err(|_| SwapSimError::SignedOverflow)?,
                )
                .ok_or(SwapSimError::AmountOverflow)?;
            amount_calculated = amount_calculated
                .checked_sub(
                    I256::try_from(
                        swap_step
                            .amount_in
                            .checked_add(swap_step.fee_amount)
                            .ok_or(SwapSimError::AmountOverflow)?,
                    )
                    .map_err(|_| SwapSimError::SignedOverflow)?,
                )
                .ok_or(SwapSimError::AmountOverflow)?;
        } else {
            amount_specified_remaining = amount_specified_remaining
                .checked_add(
                    I256::try_from(
                        swap_step
                            .amount_in
                            .checked_add(swap_step.fee_amount)
                            .ok_or(SwapSimError::AmountOverflow)?,
                    )
                    .map_err(|_| SwapSimError::SignedOverflow)?,
                )
                .ok_or(SwapSimError::AmountOverflow)?;
            amount_calculated = amount_calculated
                .checked_add(
                    I256::try_from(swap_step.amount_out)
                        .map_err(|_| SwapSimError::SignedOverflow)?,
                )
                .ok_or(SwapSimError::AmountOverflow)?;
        }

        let protocol_fee_pips = swap_fee.protocol_fee().pips(zero_for_one) as u32;

        if protocol_fee_pips != 0 {
            let protocol_delta = if result.swap_fee.pips() == protocol_fee_pips {
                step.fee_amount
            } else {
                step.amount_in
                    .checked_add(step.fee_amount)
                    .ok_or(SwapSimError::AmountOverflow)?
                    .checked_mul(U256::from(protocol_fee_pips))
                    .ok_or(SwapSimError::AmountOverflow)?
                    / U256::from(PIPS_DENOMINATOR)
            };

            step.fee_amount = step
                .fee_amount
                .checked_sub(protocol_delta)
                .ok_or(SwapSimError::AmountOverflow)?;
            result.amount_to_protocol = result
                .amount_to_protocol
                .checked_add(protocol_delta)
                .ok_or(SwapSimError::AmountOverflow)?;
        }

        if result.liquidity > Liquidity::ZERO {
            step.fee_growth_global_x128 = step
                .fee_growth_global_x128
                .checked_add(
                    crate::core::math::full::mul_div(
                        step.fee_amount,
                        U256::ONE << 128u32,
                        result.liquidity.as_u256(),
                    )
                    .map_err(|_| SwapSimError::FeeTooLarge)?,
                )
                .ok_or(SwapSimError::FeeTooLarge)?;
        }

        if result.sqrt_price_x96 == step.sqrt_price_next_x96 {
            let (fee_growth_global0_x128, fee_growth_global1_x128) = if zero_for_one {
                (
                    step.fee_growth_global_x128,
                    pool_state.fee_growth_global1_x128,
                )
            } else {
                (
                    pool_state.fee_growth_global0_x128,
                    step.fee_growth_global_x128,
                )
            };

            let mut liquidity_net = ticks.cross_tick(
                step.next_tick_indexer,
                fee_growth_global0_x128,
                fee_growth_global1_x128,
            )?;

            // Safe because valid tick liquidity cannot produce `i128::MIN`.
            if zero_for_one {
                liquidity_net = -liquidity_net;
            }

            result.liquidity = Liquidity::new(
                result
                    .liquidity
                    .value()
                    .checked_add_signed(liquidity_net)
                    .ok_or(SwapSimError::LiquidityUnderflow)?,
            );

            result.tick = if zero_for_one {
                unsafe { TickIndex::new_unchecked(step.next_tick_info.tick_index().value() - 1) }
            } else {
                step.next_tick_info.tick_index()
            };
        } else if result.sqrt_price_x96 != step.sqrt_price_start_x96 {
            result.tick = result.sqrt_price_x96.tick_index();
        }
    }

    state.set(
        result.tick,
        result.sqrt_price_x96,
        result.liquidity,
        zero_for_one,
        step.fee_growth_global_x128,
    );

    result.swap_delta = if zero_for_one == exact_in {
        let amount0 = amount_specified
            .checked_sub(amount_specified_remaining)
            .ok_or(SwapSimError::I128Overflow)?
            .try_into()
            .map_err(|_| SwapSimError::I128Overflow)?;
        let amount1 = amount_calculated
            .try_into()
            .map_err(|_| SwapSimError::I128Overflow)?;

        BalanceDelta::new(amount0, amount1)
    } else {
        let amount0 = amount_calculated
            .try_into()
            .map_err(|_| SwapSimError::I128Overflow)?;
        let amount1 = amount_specified
            .checked_sub(amount_specified_remaining)
            .ok_or(SwapSimError::I128Overflow)?
            .try_into()
            .map_err(|_| SwapSimError::I128Overflow)?;

        BalanceDelta::new(amount0, amount1)
    };

    Ok(result)
}

/// Validate that tick_lower < tick_upper and both are aligned to tick_spacing.
#[inline]
fn check_ticks(
    tick_lower: TickIndex,
    tick_upper: TickIndex,
    tick_spacing: TickSpacing,
) -> Result<(), SwapSimError> {
    if tick_lower >= tick_upper {
        return Err(SwapSimError::InvalidTick);
    }
    validate_tick_index_for_spacing(tick_lower, tick_spacing)?;
    validate_tick_index_for_spacing(tick_upper, tick_spacing)?;
    Ok(())
}

#[inline]
fn validate_tick_index_for_spacing(
    tick: TickIndex,
    tick_spacing: TickSpacing,
) -> Result<(), SwapSimError> {
    if tick.for_spacing(tick_spacing) {
        Ok(())
    } else {
        Err(SwapSimError::InvalidTick)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::core::{math::tick::get_sqrt_price_at_tick, types::tick::TickIndex};

    use super::*;
    use alloy::primitives::{Address, B256, Sign};
    use proptest::prelude::*;
    use ruint::aliases::U160;

    fn ether(n: u128) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    fn tick(index: i32) -> TickIndex {
        TickIndex::new(index).expect("tick index out of range")
    }

    fn sqrt_at(index: i32) -> SqrtPriceX96 {
        get_sqrt_price_at_tick(tick(index))
    }

    fn sqrt_from_u160(value: U160) -> SqrtPriceX96 {
        SqrtPriceX96::new(value).expect("sqrt price out of range")
    }

    fn invalid_sqrt_from_u160(value: U160) -> SqrtPriceX96 {
        unsafe { SqrtPriceX96::new_unchecked(value) }
    }

    fn sqrt_price_1_1() -> SqrtPriceX96 {
        sqrt_from_u160(U160::ONE << 96)
    }

    fn tick_spacing_60() -> TickSpacing {
        TickSpacing::new(60).unwrap()
    }

    fn test_min_sqrt_price() -> U160 {
        U160::from(4_295_128_739u64)
    }

    pub fn delta_amount_out(delta: &BalanceDelta, zero_for_one: bool) -> u128 {
        let raw = if zero_for_one {
            delta.amount1
        } else {
            delta.amount0
        };
        if raw > 0 { raw as u128 } else { 0 }
    }

    pub fn delta_amount_in(delta: &BalanceDelta, zero_for_one: bool) -> u128 {
        let raw = if zero_for_one {
            delta.amount0
        } else {
            delta.amount1
        };
        if raw < 0 { raw.unsigned_abs() } else { 0 }
    }

    fn tick_info(
        tick_idx: i32,
        liquidity_net: i128,
        liquidity_gross: u128,
    ) -> (TickIndex, TickInfo) {
        (
            tick(tick_idx),
            TickInfo::build(
                sqrt_at(tick_idx),
                tick(tick_idx),
                Liquidity::new(liquidity_gross),
                liquidity_net,
                U256::ZERO,
                U256::ZERO,
            ),
        )
    }

    fn basic_ticks() -> [(TickIndex, TickInfo); 2] {
        [
            tick_info(
                -120,
                1_000_000_000_000_000_000i128,
                1_000_000_000_000_000_000u128,
            ),
            tick_info(
                120,
                -1_000_000_000_000_000_000i128,
                1_000_000_000_000_000_000u128,
            ),
        ]
    }

    fn make_pool(
        sqrt_price_x96: SqrtPriceX96,
        tick: i32,
        liquidity: u128,
        fee: u32,
        protocol_fee: ProtocolFee,
        tick_spacing: TickSpacing,
        ticks: impl IntoIterator<Item = (TickIndex, TickInfo)>,
    ) -> Pool {
        let tick = TickIndex::new(tick).unwrap();
        let liquidity = Liquidity::new(liquidity);
        let fee = Fee::new(fee).expect("fee out of range");
        let tick_map =
            TickSlab::from_snapshot(tick_spacing, ticks.into_iter().collect::<BTreeMap<_, _>>())
                .expect("valid ticks");

        let swap_fee = SwapFee::new(fee, protocol_fee).expect("swap fee out of range");

        Pool {
            state: PoolState {
                sqrt_price_x96,
                tick,
                liquidity,
                fee_growth_global0_x128: U256::ZERO,
                fee_growth_global1_x128: U256::ZERO,
            },
            swap_fee,
            tick_spacing,
            ticks: tick_map,
            positions: (),
        }
    }

    fn basic_pool(ticks: &[(TickIndex, TickInfo)]) -> Pool {
        make_pool(
            sqrt_price_1_1(),
            0,
            1_000_000_000_000_000_000u128,
            3_000,
            ProtocolFee::ZERO,
            tick_spacing_60(),
            ticks.iter().copied(),
        )
    }

    fn modify_params(
        tick_lower: TickIndex,
        tick_upper: TickIndex,
        liquidity_delta: i128,
    ) -> ModifyLiquidityParams {
        ModifyLiquidityParams {
            tick_lower,
            tick_upper,
            liquidity_delta,
            owner: Address::ZERO,
            salt: B256::ZERO,
        }
    }

    fn make_params(
        zero_for_one: bool,
        amount: I256,
        sqrt_price_limit_x96: SqrtPriceX96,
    ) -> SwapParams {
        SwapParams {
            zero_for_one,
            amount_specified: amount,
            sqrt_price_limit_x96,
        }
    }

    fn exact_in(amount: U256) -> I256 {
        I256::checked_from_sign_and_abs(Sign::Negative, amount).unwrap()
    }

    fn exact_out(amount: U256) -> I256 {
        I256::checked_from_sign_and_abs(Sign::Positive, amount).unwrap()
    }

    #[test]
    fn swap_zero_amount_does_nothing() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        let before = pool.state;

        let result = pool
            .swap(make_params(true, exact_in(U256::ZERO), sqrt_at(-120)))
            .expect("zero-amount swap should succeed");

        assert_eq!(result.sqrt_price_x96, before.sqrt_price_x96);
        assert_eq!(result.tick, before.tick);
        assert_eq!(result.liquidity, before.liquidity);
        assert_eq!(result.swap_delta, BalanceDelta::DEFAULT);
    }

    #[test]
    fn swap_commits_state() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        let before = pool.state;
        let limit = sqrt_at(-120);

        let result = pool
            .swap(make_params(true, exact_in(ether(1)), limit))
            .expect("swap should succeed");

        let after = pool.state;
        assert_ne!(after.sqrt_price_x96, before.sqrt_price_x96);
        assert_eq!(after.sqrt_price_x96, result.sqrt_price_x96);
        assert_eq!(after.tick, result.tick);
        assert_eq!(after.liquidity, result.liquidity);
    }

    #[test]
    fn forked_swap_does_not_change_original_pool_state() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let before = pool.state;
        let limit = sqrt_at(-120);
        let mut fork = pool.try_fork().expect("pool snapshot should fork");

        let result = fork
            .swap(make_params(true, exact_in(ether(1)), limit))
            .expect("forked swap should succeed");

        let after = pool.state;
        assert_eq!(after.sqrt_price_x96, before.sqrt_price_x96);
        assert_eq!(after.tick, before.tick);
        assert_eq!(after.liquidity, before.liquidity);
        assert_ne!(result.sqrt_price_x96, before.sqrt_price_x96);
    }

    #[test]
    fn independent_swaps_match_final_result() {
        let ticks = basic_ticks();
        let mut first = basic_pool(&ticks);
        let mut second = basic_pool(&ticks);
        let params = make_params(true, exact_in(ether(1)), sqrt_at(-120));

        let first_result = first.swap(params).expect("first swap should succeed");
        let second_result = second.swap(params).expect("second swap should succeed");

        assert_eq!(first_result.sqrt_price_x96, second_result.sqrt_price_x96);
        assert_eq!(first_result.tick, second_result.tick);
        assert_eq!(first_result.liquidity, second_result.liquidity);
        assert_eq!(first_result.swap_delta, second_result.swap_delta);
    }

    #[test]
    fn independent_exact_output_swaps_match_final_result() {
        let ticks = basic_ticks();
        let mut first = basic_pool(&ticks);
        let mut second = basic_pool(&ticks);
        let params = make_params(
            true,
            exact_out(U256::from(1_000_000_000_000u128)),
            sqrt_at(-120),
        );

        let first_result = first
            .swap(params)
            .expect("first exact-output swap should succeed");
        let second_result = second
            .swap(params)
            .expect("second exact-output swap should succeed");

        assert_eq!(first_result.sqrt_price_x96, second_result.sqrt_price_x96);
        assert_eq!(first_result.tick, second_result.tick);
        assert_eq!(first_result.liquidity, second_result.liquidity);
        assert_eq!(first_result.swap_delta, second_result.swap_delta);
    }

    #[test]
    fn zero_for_one_exact_in_delta_signs() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        let limit = sqrt_at(-120);

        let result = pool
            .swap(make_params(true, exact_in(ether(1)), limit))
            .expect("swap should succeed");

        // caller / PoolManager accounting perspective:
        // caller owes token0, receives token1
        assert!(result.swap_delta.amount0 < 0, "caller owes token0");
        assert!(result.swap_delta.amount1 > 0, "caller receives token1");

        assert!(delta_amount_in(&result.swap_delta, true) > 0);
        assert!(delta_amount_out(&result.swap_delta, true) > 0);
    }

    #[test]
    fn one_for_zero_exact_in_delta_signs() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        let limit = sqrt_at(120);

        let result = pool
            .swap(make_params(false, exact_in(ether(1)), limit))
            .expect("swap should succeed");

        // caller / PoolManager accounting perspective:
        // caller receives token0, owes token1
        assert!(result.swap_delta.amount1 < 0, "caller owes token1");
        assert!(result.swap_delta.amount0 > 0, "caller receives token0");

        assert!(delta_amount_in(&result.swap_delta, false) > 0);
        assert!(delta_amount_out(&result.swap_delta, false) > 0);
    }
    #[test]
    fn zero_for_one_price_moves_down() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        let limit = sqrt_at(-120);

        let before_price = pool.state.sqrt_price_x96;
        let result = pool
            .swap(make_params(true, exact_in(ether(1)), limit))
            .unwrap();

        assert!(result.sqrt_price_x96 <= before_price, "price must not rise");
        assert!(result.sqrt_price_x96 >= limit, "price must not cross limit");
    }

    #[test]
    fn one_for_zero_price_moves_up() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        let limit = sqrt_at(120);

        let before_price = pool.state.sqrt_price_x96;
        let result = pool
            .swap(make_params(false, exact_in(ether(1)), limit))
            .unwrap();

        assert!(result.sqrt_price_x96 >= before_price, "price must not fall");
        assert!(result.sqrt_price_x96 <= limit, "price must not cross limit");
    }

    #[test]
    fn amount_in_and_out_are_consistent_with_raw_delta() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        let limit = sqrt_at(-120);

        let result = pool
            .swap(make_params(true, exact_in(ether(1)), limit))
            .unwrap();

        // For zeroForOne: input = token0 (amount0 < 0), output = token1 (amount1 > 0).
        assert_eq!(
            delta_amount_in(&result.swap_delta, true),
            result.swap_delta.amount0.unsigned_abs()
        );
        assert_eq!(
            delta_amount_out(&result.swap_delta, true),
            result.swap_delta.amount1.unsigned_abs()
        );
    }

    #[test]
    fn delta_amount_out_uses_positive_output_field() {
        let delta = BalanceDelta {
            amount0: -100,
            amount1: -50,
        };

        // zeroForOne output is token1.
        // amount1 < 0 means caller owes token1, not receives it.
        assert_eq!(delta_amount_out(&delta, true), 0);

        let delta = BalanceDelta {
            amount0: 100,
            amount1: -50,
        };

        // oneForZero output is token0.
        // amount0 > 0 means caller receives token0.
        assert_eq!(delta_amount_out(&delta, false), 100);
    }

    #[test]
    fn delta_amount_in_uses_negative_input_field() {
        let delta = BalanceDelta {
            amount0: 10,
            amount1: -20,
        };

        // zeroForOne input is token0.
        // amount0 > 0 means caller receives token0, not owes it.
        assert_eq!(delta_amount_in(&delta, true), 0);

        let delta = BalanceDelta {
            amount0: 10,
            amount1: -20,
        };

        // oneForZero input is token1.
        // amount1 < 0 means caller owes token1.
        assert_eq!(delta_amount_in(&delta, false), 20);
    }

    #[test]
    fn exact_out_never_exceeds_requested() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        let requested = 1_000_000_000_000u128;

        let result = pool
            .swap(make_params(
                true,
                exact_out(U256::from(requested)),
                sqrt_at(-120),
            ))
            .expect("exact-out swap should succeed");

        assert!(
            delta_amount_out(&result.swap_delta, true) <= requested,
            "pool must never send more than was requested"
        );
    }

    #[test]
    fn crossing_swap_updates_final_liquidity() {
        let liq = 500_000_000_000_000_000u128;
        let mut pool = one_range_pool(liq);

        let result = pool
            .swap(make_params(true, exact_in(ether(100)), sqrt_at(-240)))
            .unwrap();

        assert!(result.tick <= tick(-120), "must have crossed tick -120");
        assert_eq!(result.liquidity, Liquidity::ZERO);
        assert!(delta_amount_in(&result.swap_delta, true) > 0);
    }

    #[test]
    fn small_swap_stays_inside_current_range() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);

        // A small swap should stay inside the current range.
        let result = pool
            .swap(make_params(
                true,
                exact_in(U256::from(1_000u128)),
                sqrt_at(-1),
            ))
            .unwrap();

        assert!(result.tick > tick(-120));
        assert_eq!(result.liquidity, Liquidity::new(1_000_000_000_000_000_000));
    }

    #[test]
    fn exact_output_swap_respects_requested_output() {
        let liq = 500_000_000_000_000_000u128;
        let mut pool = one_range_pool(liq);
        let requested = 1_000_000u128;

        let result = pool
            .swap(make_params(
                true,
                exact_out(U256::from(requested)),
                sqrt_at(-240),
            ))
            .unwrap();

        assert!(delta_amount_out(&result.swap_delta, true) <= requested);
    }

    fn one_range_pool(liq: u128) -> Pool {
        make_pool(
            sqrt_at(0),
            0,
            liq,
            3_000,
            ProtocolFee::ZERO,
            tick_spacing_60(),
            [
                tick_info(-120, liq as i128, liq),
                tick_info(120, -(liq as i128), liq),
            ],
        )
    }

    #[test]
    fn crossing_tick_downward_drains_liquidity() {
        let liq = 500_000_000_000_000_000u128;
        let mut pool = one_range_pool(liq);

        let result = pool
            .swap(make_params(true, exact_in(ether(100)), sqrt_at(-240)))
            .expect("swap should succeed");

        assert!(result.tick <= tick(-120), "must have crossed tick -120");
        assert_eq!(
            result.liquidity,
            Liquidity::ZERO,
            "no liquidity below range"
        );
    }

    #[test]
    fn crossing_tick_upward_drains_liquidity() {
        let liq = 500_000_000_000_000_000u128;
        let mut pool = one_range_pool(liq);

        let result = pool
            .swap(make_params(false, exact_in(ether(100)), sqrt_at(240)))
            .expect("swap should succeed");

        assert!(result.tick >= tick(120), "must have crossed tick 120");
        assert_eq!(
            result.liquidity,
            Liquidity::ZERO,
            "no liquidity above range"
        );
    }

    #[test]
    fn add_liquidity_inside_range() {
        let mut pool = make_pool(
            sqrt_price_1_1(),
            0,
            0,
            3_000,
            ProtocolFee::ZERO,
            tick_spacing_60(),
            [],
        );
        let liq = 1_000_000_000_000u128;

        let result = pool
            .modify_liquidity(modify_params(tick(-120), tick(120), liq as i128))
            .expect("add should succeed");

        assert_eq!(pool.state.liquidity, Liquidity::new(liq));
        // Liquidity delta is caller / PoolManager perspective: caller owes tokens when LP adds.
        assert!(result.delta.amount0 < 0, "caller pays token0");
        assert!(result.delta.amount1 < 0, "caller pays token1");

        let ticks = &pool.ticks;
        assert_eq!(
            ticks.get(ticks.indexer(tick(-120))).unwrap().liquidity_net,
            liq as i128
        );
        assert_eq!(
            ticks.get(ticks.indexer(tick(120))).unwrap().liquidity_net,
            -(liq as i128)
        );
    }

    #[test]
    fn add_liquidity_below_range_only_token0() {
        let mut pool = make_pool(
            sqrt_at(-240),
            -240,
            0,
            3_000,
            ProtocolFee::ZERO,
            tick_spacing_60(),
            [],
        );

        let result = pool
            .modify_liquidity(modify_params(tick(-120), tick(120), 1_000_000_000_000i128))
            .expect("add should succeed");

        assert_eq!(
            pool.state.liquidity,
            Liquidity::ZERO,
            "out-of-range position"
        );

        // Price below range: adding liquidity requires only token0 from the caller.
        assert!(result.delta.amount0 < 0, "caller pays token0");
        assert_eq!(result.delta.amount1, 0);
    }

    #[test]
    fn remove_liquidity_clears_ticks() {
        let mut pool = make_pool(
            sqrt_price_1_1(),
            0,
            0,
            3_000,
            ProtocolFee::ZERO,
            tick_spacing_60(),
            [],
        );
        let liq = 1_000_000_000_000i128;

        pool.modify_liquidity(modify_params(tick(-120), tick(120), liq))
            .unwrap();

        let result = pool
            .modify_liquidity(modify_params(tick(-120), tick(120), -liq))
            .expect("remove should succeed");

        assert_eq!(pool.state.liquidity, Liquidity::ZERO);
        // Removing liquidity means the caller receives tokens back from the pool.
        assert!(result.delta.amount0 > 0, "caller receives token0");
        assert!(result.delta.amount1 > 0, "caller receives token1");

        let ticks = &pool.ticks;
        assert!(ticks.get(ticks.indexer(tick(-120))).is_none());
        assert!(ticks.get(ticks.indexer(tick(120))).is_none());
    }

    #[test]
    #[should_panic(expected = "tick index out of range")]
    fn rejects_out_of_range_tick_entry() {
        TickSlab::from_snapshot(
            tick_spacing_60(),
            [tick_info(TickIndex::MIN.value() - 1, 0, 1)].into(),
        )
        .unwrap_err();
    }

    #[test]
    fn exact_input_allows_100_percent_fee() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        pool.swap_fee = SwapFee::new(Fee::new(1_000_000).unwrap(), ProtocolFee::ZERO).unwrap();

        let result = pool
            .swap(make_params(true, exact_in(ether(1)), sqrt_at(-60)))
            .expect("V4 allows 100% fee for exact-input");

        // Entire input consumed as fee; no output.
        assert_eq!(
            delta_amount_in(&result.swap_delta, true),
            ether(1).to::<u128>()
        );
        assert_eq!(delta_amount_out(&result.swap_delta, true), 0);
    }

    #[test]
    fn exact_output_rejects_100_percent_fee() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        pool.swap_fee = SwapFee::new(Fee::new(1_000_000).unwrap(), ProtocolFee::ZERO).unwrap();

        assert_eq!(
            pool.swap(make_params(
                true,
                exact_out(U256::from(1_000_000u128)),
                sqrt_at(-60),
            ))
            .unwrap_err(),
            SwapSimError::MaxFeeExactOut
        );
    }

    #[test]
    fn protocol_fee_changes_effective_swap_fee_for_quotes() {
        let ticks = basic_ticks();
        let pool = make_pool(
            sqrt_price_1_1(),
            0,
            1_000_000_000_000_000_000u128,
            3_000,
            ProtocolFee::new(0, 500).unwrap(),
            tick_spacing_60(),
            ticks,
        );

        let result = pool
            .quote_swap(make_params(
                false,
                exact_in(U256::from(100_000_000u128)),
                sqrt_at(120),
            ))
            .unwrap();

        assert!(result.swap_fee.pips() > pool.swap_fee.lp_fee().pips());
        assert!(result.amount_to_protocol > U256::ZERO);
    }

    #[test]
    fn protocol_fee_above_v4_maximum_is_rejected_at_construction() {
        assert!(ProtocolFee::new(1_001, 0).is_none());
        assert!(ProtocolFee::new(0, 1_001).is_none());
    }

    #[test]
    fn rejects_limit_wrong_side_for_zero_for_one() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);

        assert_eq!(
            pool.swap(make_params(true, exact_in(ether(1)), sqrt_at(1)))
                .unwrap_err(),
            SwapSimError::PriceLimitAlreadyExceeded
        );
    }

    #[test]
    fn rejects_limit_wrong_side_for_one_for_zero() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);

        assert_eq!(
            pool.swap(make_params(false, exact_in(ether(1)), sqrt_at(-1)))
                .unwrap_err(),
            SwapSimError::PriceLimitAlreadyExceeded
        );
    }

    #[test]
    fn rejects_invalid_pool_sqrt_price() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        pool.state.sqrt_price_x96 = invalid_sqrt_from_u160(test_min_sqrt_price() - U160::ONE);

        assert_eq!(
            pool.swap(make_params(
                true,
                exact_in(ether(1)),
                sqrt_from_u160(test_min_sqrt_price()),
            ))
            .unwrap_err(),
            SwapSimError::InvalidPoolSqrtPrice
        );
    }

    proptest! {
        #[test]
        fn fuzz_price_never_crosses_limit(
            zero_for_one      in any::<bool>(),
            exact_in          in any::<bool>(),
            amount_raw        in 0u128..1_000_000_000_000_000_000u128,
            fee               in 0u32..999_999u32,
            limit_tick_offset in 1i32..500i32,
        ) {
            let liq = 1_000_000_000_000_000_000u128;
            let ticks = [
                tick_info(-600, liq as i128, liq),
                tick_info(600, -(liq as i128), liq),
            ];
            let mut pool = make_pool(
                sqrt_at(0),
                0,
                liq,
                fee,
                ProtocolFee::ZERO,
                tick_spacing_60(),
                ticks,
            );

            let limit_tick = if zero_for_one { -limit_tick_offset } else { limit_tick_offset };
            let limit = sqrt_at(limit_tick);
            let amount = if exact_in {
                self::exact_in(U256::from(amount_raw))
            } else {
                exact_out(U256::from(amount_raw))
            };

            let result = pool
                .swap(make_params(zero_for_one, amount, limit))
                .expect("swap should succeed");

            let before_price = sqrt_at(0);
            if zero_for_one {
                prop_assert!(result.sqrt_price_x96 <= before_price, "price rose on zeroForOne");
                prop_assert!(result.sqrt_price_x96 >= limit, "price crossed limit going down");
            } else {
                prop_assert!(result.sqrt_price_x96 >= before_price, "price fell on oneForZero");
                prop_assert!(result.sqrt_price_x96 <= limit, "price crossed limit going up");
            }
        }

        #[test]
        fn fuzz_amount_in_equals_raw_delta(
            amount_raw in 1u128..1_000_000_000_000_000_000u128,
            zero_for_one in any::<bool>(),
        ) {
            let liq = 1_000_000_000_000_000_000u128;
            let ticks = [
                tick_info(-600, liq as i128, liq),
                tick_info(600, -(liq as i128), liq),
            ];
            let mut pool = make_pool(
                sqrt_at(0),
                0,
                liq,
                3_000,
                ProtocolFee::ZERO,
                tick_spacing_60(),
                ticks,
            );

            let result = pool
                .swap(make_params(
                    zero_for_one,
                    exact_in(U256::from(amount_raw)),
                    SqrtPriceX96::extreme_price_limit(zero_for_one),
                ))
                .expect("swap should succeed");

            // amount_in must equal the unsigned abs of the negative input raw delta
            // (caller perspective: negative = caller owes/pays).
            if zero_for_one {
                prop_assert_eq!(
                    delta_amount_in(&result.swap_delta, true),
                    result.swap_delta.amount0.unsigned_abs(),
                    "amount_in(true) must equal |amount0|"
                );
            } else {
                prop_assert_eq!(
                    delta_amount_in(&result.swap_delta, false),
                    result.swap_delta.amount1.unsigned_abs(),
                    "amount_in(false) must equal |amount1|"
                );
            }
        }

        #[test]
        fn fuzz_exact_out_never_exceeds_requested(
            amount_raw in 1u128..1_000_000_000_000_000_000u128,
            zero_for_one in any::<bool>(),
        ) {
            let liq = 1_000_000_000_000_000_000u128;
            let ticks = [
                tick_info(-600, liq as i128, liq),
                tick_info(600, -(liq as i128), liq),
            ];
            let mut pool = make_pool(
                sqrt_at(0),
                0,
                liq,
                3_000,
                ProtocolFee::ZERO,
                tick_spacing_60(),
                ticks,
            );

            let result = pool
                .swap(make_params(
                    zero_for_one,
                    exact_out(U256::from(amount_raw)),
                    SqrtPriceX96::extreme_price_limit(zero_for_one),
                ))
                .expect("swap should succeed");

            prop_assert!(
                delta_amount_out(&result.swap_delta, zero_for_one) <= amount_raw,
                "output exceeded requested amount"
            );
        }
    }
}
