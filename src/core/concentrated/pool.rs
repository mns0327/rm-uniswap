//! Uniswap V4 pool simulator with full tick-crossing support.
//!
//! The swap loop follows V4 `Pool.sol`: it advances through initialized ticks,
//! updates liquidity on crossings, and returns deltas from the caller /
//! PoolManager accounting perspective. Negative delta values mean the caller
//! owes that token; positive values mean the caller receives it.
//!
//! `SwapParams::amount` uses V4 `amountSpecified` semantics: negative for exact
//! input and positive for exact output. Tick spacing is supplied from the
//! `PoolKey` and validated together with pool snapshots and tick stores.
//!
//! Read-only quote methods leave state unchanged. [`Pool::swap`] commits the
//! resulting price, tick, liquidity, fee growth, and crossed tick fee state only
//! after the full simulation succeeds.

use std::sync::Arc;

use parking_lot::RwLock;
use ruint::aliases::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error as SwapSimError;
use crate::core::types::liquidity::Liquidity;
use crate::core::types::nonzero::NonZeroLiquidity;
use crate::core::types::sqrt_price::SqrtPriceX96;
use crate::core::types::tick::TickIndex;
use crate::core::types::tick_spacing::TickSpacing;
use crate::core::{
    math::{
        small_ratio::{mul_div_u32_ceil, mul_div_u32_floor},
        sqrt_price::{
            get_amount0_delta, get_amount1_delta, get_next_sqrt_price_from_input,
            get_next_sqrt_price_from_output,
        },
        swap::{SwapMathError, get_sqrt_price_target},
        tick::get_tick_at_sqrt_price,
    },
    types::{
        NextInitializedTick, PoolTicksSnapshot, TickInfo, delta::BalanceDelta,
        signed::I256 as SignedAmount,
    },
};

use super::{
    cache::PoolCache,
    price::{PriceCache, sqrt_price_x96_to_price},
    ticks::ticks::{PoolTicks, PoolTicksReadGuard, PoolTicksWriteGuard},
};

/// Maximum number of swap-loop iterations per simulation.
///
/// Uniswap V4 itself has no library-level iteration cap; the EVM gas limit is
/// the practical bound. This simulator's defensive ceiling prevents malformed
/// local state from spinning indefinitely. 10 000 is far beyond any realistic
/// single swap on mainnet tick densities.
pub const MAX_SWAP_ITERATIONS: usize = 10_000;

/// Maximum swap fee in pips (100 % = 1 000 000).
const MAX_SWAP_FEE: u32 = 1_000_000;
#[cfg(test)]
const MIN_TICK: i32 = TickIndex::MIN.value();

/// Trait for tick-crossing guards used in swap simulations.
///
/// The default `next_initialized_tick_within_one_word` implementation preserves
/// the current ordered-map behavior. A future bitmap-backed `PoolTicks` guard can
/// override only that method and immediately speed up the swap loop without
/// changing the executor.
pub trait TickCrossing {
    fn next_initialized_tick(
        &self,
        current_tick: TickIndex,
        zero_for_one: bool,
    ) -> NextInitializedTick;
    fn cross_tick(&self, tick_next: TickIndex) -> Result<i128, SwapSimError>;

    #[inline(always)]
    fn next_initialized_tick_within_one_word(
        &self,
        current_tick: TickIndex,
        tick_spacing: TickSpacing,
        zero_for_one: bool,
    ) -> NextInitializedTick {
        next_initialized_tick_within_one_word_fallback(
            self,
            current_tick,
            tick_spacing,
            zero_for_one,
        )
    }
}

impl TickCrossing for PoolTicksReadGuard<'_> {
    #[inline]
    fn next_initialized_tick(
        &self,
        current_tick: TickIndex,
        zero_for_one: bool,
    ) -> NextInitializedTick {
        PoolTicksReadGuard::next_initialized_tick(self, current_tick, zero_for_one)
    }

    #[inline]
    fn cross_tick(&self, tick_next: TickIndex) -> Result<i128, SwapSimError> {
        PoolTicksReadGuard::cross_tick(self, tick_next)
    }
}

impl TickCrossing for PoolTicksWriteGuard<'_> {
    #[inline]
    fn next_initialized_tick(
        &self,
        current_tick: TickIndex,
        zero_for_one: bool,
    ) -> NextInitializedTick {
        PoolTicksWriteGuard::next_initialized_tick(self, current_tick, zero_for_one)
    }

    #[inline]
    fn cross_tick(&self, tick_next: TickIndex) -> Result<i128, SwapSimError> {
        PoolTicksWriteGuard::cross_tick(self, tick_next)
    }
}

#[cfg(feature = "protocol-fee")]
mod protocol_fee_consts {
    /// `ProtocolFeeLibrary.MAX_PROTOCOL_FEE` = 1 000 (0.1 %).
    pub const MAX_PROTOCOL_FEE: u32 = 1_000;
    /// `ProtocolFeeLibrary.PIPS_DENOMINATOR` = 1 000 000.
    pub const PIPS_DENOMINATOR: u32 = 1_000_000;
}

/// Complete V4 pool state required for a full tick-crossing simulation.
///
/// # Invariants (caller-enforced)
///
/// * `tick == floor(log_√1.0001(sqrt_price_x96))`. The simulator does not
///   re-derive tick from price; a mismatch produces wrong output.
/// * `fee <= 1_000_000` — enforced on entry by [`Pool::swap`].
/// * `tick_spacing` is positive — enforced by [`TickSpacing`].
/// * All `tick_idx` values in `ticks` must be within `[MIN_TICK, MAX_TICK]` and
///   be multiples of `tick_spacing` — enforced at `PoolTicks` construction.
#[derive(Debug, Clone)]
pub struct Pool {
    pub state: Arc<RwLock<PoolState>>,

    /// Fee tier in pips (e.g. `3_000` = 0.30 %). Must be `<= 1_000_000`.
    pub fee: u32,

    /// Tick spacing from `PoolKey.tickSpacing`.
    pub tick_spacing: TickSpacing,

    /// Initialized ticks keyed by `tick_idx` in an ordered map behind a lock.
    pub ticks: PoolTicks,

    pub cache: Arc<PoolCache>,

    pub price_cache: Arc<PriceCache>,
}

impl Serialize for Pool {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.snapshot().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Pool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let snapshot = PoolSnapshot::deserialize(deserializer)?;
        Self::try_from(snapshot).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolState {
    /// Current pool sqrt price in Q64.96 fixed-point.
    pub sqrt_price_x96: SqrtPriceX96,

    /// Current tick — must equal `floor(log_√1.0001(sqrt_price_x96))`.
    pub tick: TickIndex,

    /// Active liquidity for the current tick range (`uint128` in V4).
    pub liquidity: Liquidity,

    /// All-time LP fee growth per unit of active liquidity in token0, Q128.
    #[serde(default)]
    pub fee_growth_global0_x128: U256,

    /// All-time LP fee growth per unit of active liquidity in token1, Q128.
    #[serde(default)]
    pub fee_growth_global1_x128: U256,
}

/// Owned, serializable pool snapshot.
///
/// A snapshot never shares locks or caches with a live [`Pool`]. Treat values
/// decoded from external data as untrusted and construct a runtime pool with
/// [`Pool::try_from`] so all invariants are validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSnapshot {
    pub state: PoolState,
    pub fee: u32,
    pub tick_spacing: TickSpacing,
    pub ticks: PoolTicksSnapshot,
}

impl PoolSnapshot {
    /// Validate all state that can affect swap correctness.
    pub fn validate(&self) -> Result<(), SwapSimError> {
        validate_fee(self.fee)?;

        if self.ticks.tick_spacing != self.tick_spacing {
            return Err(SwapSimError::InvalidTickSpacing);
        }

        if get_tick_at_sqrt_price(&self.state.sqrt_price_x96) != self.state.tick {
            return Err(SwapSimError::InvalidTick);
        }

        let max_liquidity_per_tick = self.tick_spacing.max_liquidity_per_tick();
        let mut active_liquidity = SignedAmount::zero();

        for (&tick_idx, info) in &self.ticks.inner {
            validate_tick_index_for_spacing(tick_idx, self.tick_spacing)?;

            if info.liquidity_gross == 0 || info.liquidity_net.unsigned_abs() > info.liquidity_gross
            {
                return Err(SwapSimError::InvalidTick);
            }

            if info.liquidity_gross > max_liquidity_per_tick.value() {
                return Err(SwapSimError::LiquidityOverflow);
            }

            if tick_idx <= self.state.tick {
                active_liquidity = active_liquidity
                    .checked_add(SignedAmount::from(info.liquidity_net))
                    .ok_or(SwapSimError::LiquidityOverflow)?;
            }
        }

        if active_liquidity.is_negative()
            || active_liquidity.abs() != self.state.liquidity.as_u256()
        {
            return Err(SwapSimError::InvalidTick);
        }

        Ok(())
    }
}

impl TryFrom<PoolSnapshot> for Pool {
    type Error = SwapSimError;

    fn try_from(snapshot: PoolSnapshot) -> Result<Self, Self::Error> {
        snapshot.validate()?;

        let ticks = PoolTicks::from_snapshot(snapshot.ticks.tick_spacing, snapshot.ticks.inner)?;
        let sqrt_price_x96 = snapshot.state.sqrt_price_x96;
        let fee = snapshot.fee;

        Ok(Self {
            state: Arc::new(RwLock::new(snapshot.state)),
            fee,
            tick_spacing: snapshot.tick_spacing,
            ticks,
            cache: Arc::new(PoolCache::default()),
            price_cache: Arc::new(PriceCache::new(
                sqrt_price_x96_to_price(sqrt_price_x96),
                fee,
            )),
        })
    }
}

/// Parameters for a single simulated swap.
///
/// Mirrors Uniswap V4's `IPoolManager.SwapParams` struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapParams {
    /// `true`  → sell token0, buy token1 (price moves **down**).
    /// `false` → sell token1, buy token0 (price moves **up**).
    pub zero_for_one: bool,

    /// Signed amount and exactness, mirroring V4's `amountSpecified`:
    ///
    /// * negative → exact-input; absolute value is the input amount.
    /// * positive → exact-output; absolute value is the desired output amount.
    pub amount: SignedAmount,

    /// Worst-acceptable sqrt price after the swap, in Q64.96.
    ///
    /// * `zero_for_one = true`  → must be in `(MIN_SQRT_PRICE, current_sqrt_price)`.
    /// * `zero_for_one = false` → must be in `(current_sqrt_price, MAX_SQRT_PRICE)`.
    pub sqrt_price_limit_x96: SqrtPriceX96,

    /// Per-swap protocol fee in pips. Only meaningful with `feature = "protocol-fee"`.
    #[cfg(feature = "protocol-fee")]
    pub protocol_fee: Option<u32>,
}

impl SwapParams {
    /// Construct a swap with no price limit (uses the loosest valid limit).
    pub fn new(zero_for_one: bool, amount: SignedAmount) -> Self {
        Self {
            zero_for_one,
            amount,
            sqrt_price_limit_x96: extreme_price_limit(zero_for_one),
            #[cfg(feature = "protocol-fee")]
            protocol_fee: None,
        }
    }
}

/// Parameters for a liquidity position update.
///
/// Mirrors Uniswap V4's `IPoolManager.ModifyLiquidityParams`, minus owner/salt
/// and position-fee accounting (not implemented at the pool-simulator layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifyLiquidityParams {
    /// Lower tick of the position range.
    pub tick_lower: TickIndex,
    /// Upper tick of the position range.
    pub tick_upper: TickIndex,
    /// Signed liquidity delta. Positive adds liquidity, negative removes it.
    pub liquidity_delta: i128,
}

impl ModifyLiquidityParams {
    pub fn new(tick_lower: TickIndex, tick_upper: TickIndex, liquidity_delta: i128) -> Self {
        Self {
            tick_lower,
            tick_upper,
            liquidity_delta,
        }
    }
}

/// Output of a liquidity position update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifyLiquidityResult {
    /// Token deltas for the pool's token balances from the liquidity change.
    ///
    /// Positive → pool receives tokens from the LP.
    /// Negative → pool sends tokens back to the LP.
    pub delta: BalanceDelta,

    /// Active pool liquidity after the update.
    pub liquidity: Liquidity,

    /// Whether the lower tick flipped initialized/uninitialized state.
    pub flipped_lower: bool,

    /// Whether the upper tick flipped initialized/uninitialized state.
    pub flipped_upper: bool,

    /// Fee growth inside the position range after the position update.
    pub fee_growth_inside0_x128: U256,

    /// Fee growth inside the position range after the position update.
    pub fee_growth_inside1_x128: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickCrossInfo {
    pub cumulative_input: U256,
    pub tick: TickIndex,
    pub liquidity_after: Liquidity,
    pub fee_growth_global0_x128: U256,
    pub fee_growth_global1_x128: U256,
}

/// Output of a successful simulated swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullSwapResult {
    /// Pool sqrt price after the swap, in Q64.96.
    pub sqrt_price_x96: SqrtPriceX96,

    /// Pool tick after the swap.
    pub tick: TickIndex,

    /// Active liquidity after the swap. May differ from the input value
    /// if tick boundaries were crossed.
    pub liquidity: Liquidity,

    pub fee_growth_global0_x128: U256,

    pub fee_growth_global1_x128: U256,

    /// Signed token deltas in V4's caller / PoolManager `BalanceDelta` convention.
    ///
    /// Use [`delta_amount_in`] / [`delta_amount_out`] for
    /// direction-aware unsigned magnitudes.
    pub delta: BalanceDelta,

    pub crossings: Vec<TickCrossInfo>,

    /// Protocol fee collected in the input token, in raw token units.
    ///
    /// Only present when compiled with `feature = "protocol-fee"`. Matches
    /// Uniswap V4's `uint128` type for the protocol fee amount.
    #[cfg(feature = "protocol-fee")]
    pub protocol_fee_amount: u128,
}

/// Lightweight swap simulation result used by the hot quote path.
///
/// This intentionally omits `crossings` so the default read-only simulator does
/// not allocate or record debug-only crossing metadata. Use
/// [`Pool::simulate_swap_full`] when you need crossing details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapSimulationResult {
    /// Pool sqrt price after the simulated swap, in Q64.96.
    pub sqrt_price_x96: SqrtPriceX96,

    /// Pool tick after the simulated swap.
    pub tick: TickIndex,

    /// Active liquidity after the simulated swap.
    pub liquidity: Liquidity,

    pub fee_growth_global0_x128: U256,

    pub fee_growth_global1_x128: U256,

    /// Signed token deltas in V4's caller / PoolManager `BalanceDelta` convention.
    pub delta: BalanceDelta,

    /// Protocol fee collected in the input token, in raw token units.
    #[cfg(feature = "protocol-fee")]
    pub protocol_fee_amount: u128,
}

impl SwapSimulationResult {
    #[inline]
    fn into_full(self, crossings: Vec<TickCrossInfo>) -> FullSwapResult {
        FullSwapResult {
            sqrt_price_x96: self.sqrt_price_x96,
            tick: self.tick,
            liquidity: self.liquidity,
            fee_growth_global0_x128: self.fee_growth_global0_x128,
            fee_growth_global1_x128: self.fee_growth_global1_x128,
            delta: self.delta,
            crossings,
            #[cfg(feature = "protocol-fee")]
            protocol_fee_amount: self.protocol_fee_amount,
        }
    }
}

/// Prepared immutable swap context.
///
/// `prepare_swap` validates fee/tick-spacing once and holds the tick read guard
/// for ordered traversal. The executor receives the already-computed effective
/// swap fee, avoiding duplicate validation and protocol-fee calculation.
struct PreparedSwap<'a> {
    ticks: PoolTicksReadGuard<'a>,
    effective_fee: u32,
    cache: &'a PoolCache,
}

impl Pool {
    /// Construct a pool after validating every state invariant.
    ///
    /// Use this for external or dynamically assembled state. The legacy
    /// [`Pool::new`] constructor remains available for trusted, already
    /// validated state.
    pub fn try_new(
        sqrt_price_x96: SqrtPriceX96,
        tick: TickIndex,
        liquidity: Liquidity,
        fee: u32,
        tick_spacing: TickSpacing,
        ticks: PoolTicks,
    ) -> Result<Self, SwapSimError> {
        PoolSnapshot {
            state: PoolState {
                sqrt_price_x96,
                tick,
                liquidity,
                fee_growth_global0_x128: U256::ZERO,
                fee_growth_global1_x128: U256::ZERO,
            },
            fee,
            tick_spacing,
            ticks: ticks.owned_snapshot(),
        }
        .try_into()
    }

    /// Construct a pool from trusted state without eager invariant validation.
    ///
    /// Prefer [`Pool::try_new`] for external input.
    pub fn new(
        sqrt_price_x96: SqrtPriceX96,
        tick: TickIndex,
        liquidity: Liquidity,
        fee: u32,
        tick_spacing: TickSpacing,
        ticks: PoolTicks,
    ) -> Self {
        let cache = Arc::new(PoolCache::default());

        Pool {
            state: Arc::new(RwLock::new(PoolState {
                sqrt_price_x96,
                tick,
                liquidity,
                fee_growth_global0_x128: U256::ZERO,
                fee_growth_global1_x128: U256::ZERO,
            })),
            fee,
            tick_spacing,
            ticks,
            cache,
            price_cache: Arc::new(PriceCache::new(
                sqrt_price_x96_to_price(sqrt_price_x96),
                fee,
            )),
        }
    }

    /// Return another handle to the same shared pool state.
    ///
    /// This is equivalent to [`Clone`] and is provided to make shared-state
    /// intent explicit at call sites.
    #[inline]
    #[must_use]
    pub fn shared_handle(&self) -> Self {
        self.clone()
    }

    /// Capture an owned, internally consistent snapshot.
    ///
    /// The tick lock is acquired before the state lock, matching mutation APIs.
    pub fn snapshot(&self) -> PoolSnapshot {
        let ticks = self.ticks.read();
        let state = *self.state.read();

        PoolSnapshot {
            state,
            fee: self.fee,
            tick_spacing: self.tick_spacing,
            ticks: PoolTicksSnapshot {
                tick_spacing: self.ticks.tick_spacing(),
                inner: ticks.snapshot(),
            },
        }
    }

    /// Create an independent pool with fresh locks and caches.
    pub fn try_fork(&self) -> Result<Self, SwapSimError> {
        Self::try_from(self.snapshot())
    }

    /// Return the fee-adjusted spot price used for route scoring.
    ///
    /// Exact swap accounting never uses this `f64` cache.
    #[inline]
    #[must_use]
    pub fn spot_price_with_fee(&self, zero_for_one: bool) -> f64 {
        self.price_cache.get_price_with_fee(zero_for_one)
    }

    /// Apply a liquidity change and commit it to pool state.
    ///
    /// Implements the pool-level portion of Uniswap V4's `modifyLiquidity`:
    ///
    /// 1. Validate `tick_lower < tick_upper` and tick spacing alignment.
    /// 2. Update lower/upper ticks with the signed `liquidity_net` convention.
    /// 3. Enforce `TickSpacing::max_liquidity_per_tick` when adding.
    /// 4. Compute the token delta owed by/to the pool.
    /// 5. Update active liquidity when the current tick is inside the range.
    ///
    /// Position ownership, fee-growth-inside, and fee collection are not
    /// implemented here (this layer does not store a `positions` mapping or
    /// fee-growth globals).
    pub fn modify_liquidity(
        &self,
        params: ModifyLiquidityParams,
    ) -> Result<ModifyLiquidityResult, SwapSimError> {
        if self.ticks.tick_spacing() != self.tick_spacing {
            return Err(SwapSimError::InvalidTickSpacing);
        }
        check_ticks(params.tick_lower, params.tick_upper, self.tick_spacing)?;

        // Lock order: ticks first, then pool state — must match Pool::swap to
        // prevent deadlock if both are called concurrently.
        let mut ticks = self.ticks.write();
        let mut state = self.state.write();
        let delta = liquidity_principal_delta(&state, &self.cache, params)?;
        let liquidity_after = if params.liquidity_delta != 0
            && state.tick >= params.tick_lower
            && state.tick < params.tick_upper
        {
            add_liquidity_delta(state.liquidity, params.liquidity_delta)?
        } else {
            state.liquidity
        };

        let mut flipped_lower = false;
        let mut flipped_upper = false;
        let fee_growth_before = fee_growth_inside_from_ticks(
            &ticks,
            state.tick,
            params.tick_lower,
            params.tick_upper,
            state.fee_growth_global0_x128,
            state.fee_growth_global1_x128,
        );

        if params.liquidity_delta != 0 {
            let lower_was_uninitialized = ticks.get(params.tick_lower).is_none();
            let upper_was_uninitialized = ticks.get(params.tick_upper).is_none();
            let max_liquidity_per_tick = if params.liquidity_delta > 0 {
                Some(self.tick_spacing.max_liquidity_per_tick())
            } else {
                None
            };

            let (lower, upper) = ticks.update_tick_pair(
                params.tick_lower,
                params.tick_upper,
                params.liquidity_delta,
                max_liquidity_per_tick,
            )?;

            flipped_lower = lower.flipped;
            flipped_upper = upper.flipped;
            ticks.initialize_fee_growth_if_needed(
                params.tick_lower,
                lower_was_uninitialized,
                state.tick,
                state.fee_growth_global0_x128,
                state.fee_growth_global1_x128,
            );
            ticks.initialize_fee_growth_if_needed(
                params.tick_upper,
                upper_was_uninitialized,
                state.tick,
                state.fee_growth_global0_x128,
                state.fee_growth_global1_x128,
            );
        }

        state.liquidity = liquidity_after;

        self.cache.clear_cross_amounts();

        let (fee_growth_inside0_x128, fee_growth_inside1_x128) = if params.liquidity_delta < 0 {
            fee_growth_before
        } else {
            fee_growth_inside_from_ticks(
                &ticks,
                state.tick,
                params.tick_lower,
                params.tick_upper,
                state.fee_growth_global0_x128,
                state.fee_growth_global1_x128,
            )
        };

        Ok(ModifyLiquidityResult {
            delta,
            liquidity: state.liquidity,
            flipped_lower,
            flipped_upper,
            fee_growth_inside0_x128,
            fee_growth_inside1_x128,
        })
    }

    /// Return current all-time fee growth inside a tick range.
    pub fn fee_growth_inside(
        &self,
        tick_lower: TickIndex,
        tick_upper: TickIndex,
    ) -> Result<(U256, U256), SwapSimError> {
        check_ticks(tick_lower, tick_upper, self.tick_spacing)?;
        let ticks = self.ticks.read();
        let state = self.state.read();
        Ok(fee_growth_inside_from_ticks(
            &ticks,
            state.tick,
            tick_lower,
            tick_upper,
            state.fee_growth_global0_x128,
            state.fee_growth_global1_x128,
        ))
    }

    /// Quote principal token deltas for a liquidity change without mutation.
    pub fn quote_modify_liquidity(
        &self,
        params: ModifyLiquidityParams,
    ) -> Result<BalanceDelta, SwapSimError> {
        check_ticks(params.tick_lower, params.tick_upper, self.tick_spacing)?;
        let state = self.state.read();
        validate_tick_range(state.tick)?;
        liquidity_principal_delta(&state, &self.cache, params)
    }

    /// Execute a swap and commit the resulting state to this pool.
    ///
    /// This mutating API preserves the full result shape, including crossings,
    /// because callers that mutate pool state often need audit/debug metadata.
    /// For the fastest read-only quote path, use [`Pool::simulate_swap`].
    pub fn swap(&self, params: SwapParams) -> Result<FullSwapResult, SwapSimError> {
        let exact_in = params.amount.is_negative();
        #[cfg(feature = "protocol-fee")]
        let effective_fee = calculate_swap_fee(params.protocol_fee, self.fee)?;
        #[cfg(not(feature = "protocol-fee"))]
        let effective_fee = self.fee;
        validate_swap_fee_for_exactness(effective_fee, exact_in)?;
        if self.ticks.tick_spacing() != self.tick_spacing {
            return Err(SwapSimError::InvalidTickSpacing);
        }

        let mut ticks = self.ticks.write();
        let mut state = self.state.write();

        let result = execute_swap_full_from_state(
            &state,
            &ticks,
            self.cache.as_ref(),
            effective_fee,
            self.tick_spacing,
            params,
        )?;

        // Commit only after the full simulation succeeds.
        for crossing in &result.crossings {
            ticks.cross_fee_growth(
                crossing.tick,
                crossing.fee_growth_global0_x128,
                crossing.fee_growth_global1_x128,
            )?;
        }
        state.sqrt_price_x96 = result.sqrt_price_x96;
        state.tick = result.tick;
        state.fee_growth_global0_x128 = result.fee_growth_global0_x128;
        state.fee_growth_global1_x128 = result.fee_growth_global1_x128;
        if state.liquidity != result.liquidity {
            state.liquidity = result.liquidity;
        }

        self.cache.clear_cross_amounts();
        self.price_cache
            .update_price(sqrt_price_x96_to_price(result.sqrt_price_x96));

        Ok(result)
    }

    /// Lightweight read-only simulation.
    ///
    /// This is now the default hot-path simulator. It does not allocate or
    /// populate `crossings`, and it snapshots pool state before executing the
    /// loop so the state read lock is not held during the full simulation.
    pub fn simulate_swap(&self, params: SwapParams) -> Result<SwapSimulationResult, SwapSimError> {
        let prepared = self.prepare_swap(&params)?;
        let state = *self.state.read();

        execute_swap_light_from_state(
            &state,
            &prepared.ticks,
            prepared.cache,
            prepared.effective_fee,
            self.tick_spacing,
            params,
        )
    }

    /// Full read-only simulation with tick-crossing metadata.
    ///
    /// Use this for debugging, replay reports, and parity validation. Avoid it
    /// in the optimizer hot path unless `crossings` are actually required.
    pub fn simulate_swap_full(&self, params: SwapParams) -> Result<FullSwapResult, SwapSimError> {
        let prepared = self.prepare_swap(&params)?;
        let state = *self.state.read();

        execute_swap_full_from_state(
            &state,
            &prepared.ticks,
            prepared.cache,
            prepared.effective_fee,
            self.tick_spacing,
            params,
        )
    }

    /// Alias for [`Pool::simulate_swap`] when call-site semantics are quote-focused.
    #[inline]
    pub fn quote_swap(&self, params: SwapParams) -> Result<SwapSimulationResult, SwapSimError> {
        self.simulate_swap(params)
    }

    /// Quote an exact-input swap without constructing a full [`FullSwapResult`].
    ///
    /// Read-only. Returns `(amount_out, sqrt_price_x96, tick, liquidity)`.
    pub fn quote_exact_input(
        &self,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<(u128, SqrtPriceX96, TickIndex, Liquidity), SwapSimError> {
        let result = self.simulate_swap(SwapParams::new(
            zero_for_one,
            SignedAmount::negative(amount_in),
        ))?;

        Ok((
            delta_amount_out(&result.delta, zero_for_one),
            result.sqrt_price_x96,
            result.tick,
            result.liquidity,
        ))
    }

    /// Validate immutable pool configuration and lock initialized ticks.
    ///
    /// Fee validation intentionally runs first to preserve V4 revert ordering.
    #[inline]
    fn prepare_swap(&self, params: &SwapParams) -> Result<PreparedSwap<'_>, SwapSimError> {
        let exact_in = params.amount.is_negative();

        #[cfg(feature = "protocol-fee")]
        let effective_fee = calculate_swap_fee(params.protocol_fee, self.fee)?;
        #[cfg(not(feature = "protocol-fee"))]
        let effective_fee = self.fee;

        validate_swap_fee_for_exactness(effective_fee, exact_in)?;
        if self.ticks.tick_spacing() != self.tick_spacing {
            return Err(SwapSimError::InvalidTickSpacing);
        }

        Ok(PreparedSwap {
            ticks: self.ticks.read(),
            effective_fee,
            cache: self.cache.as_ref(),
        })
    }

    /// Lightweight simulation from an externally supplied state snapshot.
    pub fn simulate_swap_from_state(
        &self,
        state: &PoolState,
        params: SwapParams,
    ) -> Result<SwapSimulationResult, SwapSimError> {
        let prepared = self.prepare_swap(&params)?;

        execute_swap_light_from_state(
            state,
            &prepared.ticks,
            prepared.cache,
            prepared.effective_fee,
            self.tick_spacing,
            params,
        )
    }

    /// Full simulation from an externally supplied state snapshot.
    pub fn simulate_swap_full_from_state(
        &self,
        state: &PoolState,
        params: SwapParams,
    ) -> Result<FullSwapResult, SwapSimError> {
        let prepared = self.prepare_swap(&params)?;

        execute_swap_full_from_state(
            state,
            &prepared.ticks,
            prepared.cache,
            prepared.effective_fee,
            self.tick_spacing,
            params,
        )
    }
}

/// Mutable loop state shared by the light and full executors.
#[derive(Debug, Clone, Copy)]
struct SwapLoopState {
    sqrt_price_x96: SqrtPriceX96,
    tick: TickIndex,
    liquidity: Liquidity,
    fee_growth_global0_x128: U256,
    fee_growth_global1_x128: U256,
}

#[inline]
fn execute_swap_light_from_state<G: TickCrossing>(
    state: &PoolState,
    ticks: &G,
    cache: &PoolCache,
    effective_swap_fee: u32,
    tick_spacing: TickSpacing,
    params: SwapParams,
) -> Result<SwapSimulationResult, SwapSimError> {
    validate_tick_range(state.tick)?;
    debug_assert!(effective_swap_fee <= MAX_SWAP_FEE);

    // V4 returns immediately for zero amount after fee validation and before
    // price-limit validation. `prepare_swap` has already validated the fee.
    if params.amount.is_zero() {
        return Ok(SwapSimulationResult {
            sqrt_price_x96: state.sqrt_price_x96,
            tick: state.tick,
            liquidity: state.liquidity,
            fee_growth_global0_x128: state.fee_growth_global0_x128,
            fee_growth_global1_x128: state.fee_growth_global1_x128,
            delta: BalanceDelta::default(),
            #[cfg(feature = "protocol-fee")]
            protocol_fee_amount: 0,
        });
    }

    validate_price_limit(state, params.zero_for_one, params.sqrt_price_limit_x96)?;

    if params.amount.is_negative() {
        execute_exact_in_light_loop(
            state,
            ticks,
            cache,
            effective_swap_fee,
            tick_spacing,
            params,
        )
    } else {
        execute_exact_out_light_loop(
            state,
            ticks,
            cache,
            effective_swap_fee,
            tick_spacing,
            params,
        )
    }
}

#[inline]
fn execute_swap_full_from_state<G: TickCrossing>(
    state: &PoolState,
    ticks: &G,
    cache: &PoolCache,
    effective_swap_fee: u32,
    tick_spacing: TickSpacing,
    params: SwapParams,
) -> Result<FullSwapResult, SwapSimError> {
    let (light, crossings) = execute_swap_core::<G, true>(
        state,
        ticks,
        cache,
        effective_swap_fee,
        tick_spacing,
        params,
    )?;

    Ok(light.into_full(crossings.unwrap_or_default()))
}

fn execute_swap_core<G: TickCrossing, const RECORD_CROSSINGS: bool>(
    state: &PoolState,
    ticks: &G,
    cache: &PoolCache,
    effective_swap_fee: u32,
    tick_spacing: TickSpacing,
    params: SwapParams,
) -> Result<(SwapSimulationResult, Option<Vec<TickCrossInfo>>), SwapSimError> {
    validate_tick_range(state.tick)?;
    debug_assert!(effective_swap_fee <= MAX_SWAP_FEE);

    // V4 returns immediately for zero amount after fee validation and before
    // price-limit validation. `prepare_swap` has already validated the fee.
    if params.amount.is_zero() {
        return Ok((
            SwapSimulationResult {
                sqrt_price_x96: state.sqrt_price_x96,
                tick: state.tick,
                liquidity: state.liquidity,
                fee_growth_global0_x128: state.fee_growth_global0_x128,
                fee_growth_global1_x128: state.fee_growth_global1_x128,
                delta: BalanceDelta::default(),
                #[cfg(feature = "protocol-fee")]
                protocol_fee_amount: 0,
            },
            if RECORD_CROSSINGS {
                Some(Vec::new())
            } else {
                None
            },
        ));
    }

    validate_price_limit(state, params.zero_for_one, params.sqrt_price_limit_x96)?;

    if params.amount.is_negative() {
        execute_exact_in_loop::<G, RECORD_CROSSINGS>(
            state,
            ticks,
            cache,
            effective_swap_fee,
            tick_spacing,
            params,
        )
    } else {
        execute_exact_out_loop::<G, RECORD_CROSSINGS>(
            state,
            ticks,
            cache,
            effective_swap_fee,
            tick_spacing,
            params,
        )
    }
}

#[inline(always)]
fn checked_add_u256(a: U256, b: U256) -> Result<U256, SwapSimError> {
    a.checked_add(b).ok_or(SwapSimError::AmountOverflow)
}

#[inline(always)]
fn checked_sub_u256(a: U256, b: U256) -> Result<U256, SwapSimError> {
    a.checked_sub(b).ok_or(SwapSimError::AmountOverflow)
}

#[inline(always)]
fn accrue_lp_fee(
    result: &mut SwapLoopState,
    fee_amount: U256,
    zero_for_one: bool,
) -> Result<(), SwapSimError> {
    if fee_amount.is_zero() || result.liquidity.is_zero() {
        return Ok(());
    }

    let growth = crate::core::math::full::mul_div(
        fee_amount,
        U256::ONE << 128u32,
        result.liquidity.as_u256(),
    )
    .map_err(|_| SwapSimError::AmountOverflow)?;

    if zero_for_one {
        result.fee_growth_global0_x128 = result.fee_growth_global0_x128.wrapping_add(growth);
    } else {
        result.fee_growth_global1_x128 = result.fee_growth_global1_x128.wrapping_add(growth);
    }
    Ok(())
}

#[inline(always)]
fn make_delta_from_exact_in(
    zero_for_one: bool,
    input_paid_by_caller: U256,
    output_received_by_caller: U256,
) -> Result<BalanceDelta, SwapSimError> {
    // V4 `Pool.swap` returns the caller / PoolManager accounting delta:
    // negative = caller owes/sends token, positive = caller receives token.
    let input_i128 = u256_magnitude_to_i128(input_paid_by_caller, true)?;
    let output_i128 = u256_magnitude_to_i128(output_received_by_caller, false)?;

    Ok(if zero_for_one {
        BalanceDelta {
            amount0: input_i128,
            amount1: output_i128,
        }
    } else {
        BalanceDelta {
            amount0: output_i128,
            amount1: input_i128,
        }
    })
}

#[inline(always)]
fn make_delta_from_exact_out(
    zero_for_one: bool,
    input_paid_by_caller: U256,
    output_received_by_caller: U256,
) -> Result<BalanceDelta, SwapSimError> {
    // Exact-output has the same final delta convention as exact-input:
    // input token is negative, output token is positive.
    let input_i128 = u256_magnitude_to_i128(input_paid_by_caller, true)?;
    let output_i128 = u256_magnitude_to_i128(output_received_by_caller, false)?;

    Ok(if zero_for_one {
        BalanceDelta {
            amount0: input_i128,
            amount1: output_i128,
        }
    } else {
        BalanceDelta {
            amount0: output_i128,
            amount1: input_i128,
        }
    })
}

#[inline(always)]
pub fn delta_amount_out(delta: &BalanceDelta, zero_for_one: bool) -> u128 {
    let raw = if zero_for_one {
        delta.amount1
    } else {
        delta.amount0
    };
    if raw > 0 { raw as u128 } else { 0 }
}

#[inline(always)]
pub fn delta_amount_in(delta: &BalanceDelta, zero_for_one: bool) -> u128 {
    let raw = if zero_for_one {
        delta.amount0
    } else {
        delta.amount1
    };
    if raw < 0 { raw.unsigned_abs() } else { 0 }
}

/// Minimal swap-step output used by the lightweight simulator.
///
/// This deliberately mirrors only the fields the pool loop needs and avoids
/// re-entering the generic `compute_swap_step` dispatcher from the hot path.
#[derive(Debug, Clone, Copy)]
struct LightSwapStep {
    sqrt_ratio_next_x96: SqrtPriceX96,
    amount_in: U256,
    amount_out: U256,
    fee_amount: U256,
}

#[inline(always)]
fn amount_less_fee_exact_in_fast(
    amount_remaining: U256,
    fee_pips: u32,
) -> Result<U256, SwapMathError> {
    if fee_pips == 0 {
        return Ok(amount_remaining);
    }
    if fee_pips == MAX_SWAP_FEE {
        return Ok(U256::ZERO);
    }

    mul_div_u32_floor(amount_remaining, MAX_SWAP_FEE - fee_pips, MAX_SWAP_FEE)
}

#[inline(always)]
fn fee_on_exact_input_fast(amount_in: U256, fee_pips: u32) -> Result<U256, SwapMathError> {
    if amount_in.is_zero() || fee_pips == 0 {
        return Ok(U256::ZERO);
    }
    if fee_pips == MAX_SWAP_FEE {
        return Ok(amount_in);
    }

    mul_div_u32_ceil(amount_in, fee_pips, MAX_SWAP_FEE - fee_pips)
}

/// Exact-input swap-step calculation for the lightweight pool loop.
///
/// The public `compute_swap_step` must parse a signed amount and dispatch to
/// exact-in/exact-out every call. The simulator loop already knows exactness,
/// direction, fee, and liquidity, so this function skips that generic layer.
#[inline]
fn compute_swap_step_exact_in_fast(
    sqrt_ratio_current_x96: SqrtPriceX96,
    sqrt_ratio_target_x96: SqrtPriceX96,
    liquidity: Liquidity,
    amount_remaining: U256,
    fee_pips: u32,
    zero_for_one: bool,
) -> Result<LightSwapStep, SwapMathError> {
    let Some(liquidity) = NonZeroLiquidity::new(liquidity.value()) else {
        return Ok(LightSwapStep {
            sqrt_ratio_next_x96: sqrt_ratio_target_x96,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            fee_amount: U256::ZERO,
        });
    };
    let amount_remaining_less_fee = amount_less_fee_exact_in_fast(amount_remaining, fee_pips)?;

    let (max_amount_in, _) = if zero_for_one {
        get_amount0_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity,
            true,
        )?
    } else {
        get_amount1_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            true,
        )?
    };

    let reached_target = amount_remaining_less_fee >= max_amount_in;

    let (sqrt_ratio_next_x96, amount_in) = if reached_target {
        (sqrt_ratio_target_x96, max_amount_in)
    } else {
        // V4-compatible accounting: when exact-input does not reach the
        // target, the whole fee-adjusted amount is consumed as input. Do not
        // recompute amount_in from the rounded next price.
        let next = get_next_sqrt_price_from_input(
            sqrt_ratio_current_x96,
            liquidity,
            amount_remaining_less_fee,
            zero_for_one,
        )?;
        (next, amount_remaining_less_fee)
    };

    let (amount_out, _) = if zero_for_one {
        get_amount1_delta(
            sqrt_ratio_next_x96,
            sqrt_ratio_current_x96,
            liquidity,
            false,
        )?
    } else {
        get_amount0_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_next_x96,
            liquidity,
            false,
        )?
    };

    let fee_amount = if reached_target {
        fee_on_exact_input_fast(amount_in, fee_pips)?
    } else {
        amount_remaining - amount_in
    };

    Ok(LightSwapStep {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

/// Exact-output swap-step calculation for the lightweight pool loop.
#[inline]
fn compute_swap_step_exact_out_fast(
    sqrt_ratio_current_x96: SqrtPriceX96,
    sqrt_ratio_target_x96: SqrtPriceX96,
    liquidity: Liquidity,
    amount_remaining: U256,
    fee_pips: u32,
    zero_for_one: bool,
) -> Result<LightSwapStep, SwapMathError> {
    debug_assert!(fee_pips < MAX_SWAP_FEE);

    let Some(liquidity) = NonZeroLiquidity::new(liquidity.value()) else {
        return Ok(LightSwapStep {
            sqrt_ratio_next_x96: sqrt_ratio_target_x96,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            fee_amount: U256::ZERO,
        });
    };

    let (max_amount_out, _) = if zero_for_one {
        get_amount1_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity,
            false,
        )?
    } else {
        get_amount0_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            false,
        )?
    };

    let (sqrt_ratio_next_x96, amount_out) = if amount_remaining >= max_amount_out {
        (sqrt_ratio_target_x96, max_amount_out)
    } else {
        let next = get_next_sqrt_price_from_output(
            sqrt_ratio_current_x96,
            liquidity,
            amount_remaining,
            zero_for_one,
        )?;
        (next, amount_remaining)
    };

    let (amount_in, _) = if zero_for_one {
        get_amount0_delta(sqrt_ratio_next_x96, sqrt_ratio_current_x96, liquidity, true)?
    } else {
        get_amount1_delta(sqrt_ratio_current_x96, sqrt_ratio_next_x96, liquidity, true)?
    };

    let fee_amount = fee_on_exact_input_fast(amount_in, fee_pips)?;

    Ok(LightSwapStep {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

#[cfg(feature = "protocol-fee")]
#[inline(always)]
fn deduct_protocol_fee_from_step(
    amount_in: U256,
    fee_amount: &mut U256,
    effective_swap_fee: u32,
    protocol_fee: Option<u32>,
    total_protocol_fee: &mut U256,
) -> Result<(), SwapSimError> {
    use protocol_fee_consts::PIPS_DENOMINATOR;

    let Some(protocol_fee) = protocol_fee else {
        return Ok(());
    };

    if protocol_fee == 0 {
        return Ok(());
    }

    let in_plus_fee_for_protocol = checked_add_u256(amount_in, *fee_amount)?;

    let protocol_delta = if effective_swap_fee == protocol_fee {
        *fee_amount
    } else {
        mul_div_u32_floor(in_plus_fee_for_protocol, protocol_fee, PIPS_DENOMINATOR)?
    };

    *fee_amount = fee_amount
        .checked_sub(protocol_delta)
        .ok_or(SwapSimError::AmountOverflow)?;

    *total_protocol_fee = total_protocol_fee
        .checked_add(protocol_delta)
        .ok_or(SwapSimError::AmountOverflow)?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn try_apply_cached_exact_in_cross<G: TickCrossing>(
    ticks: &G,
    cache: &PoolCache,
    result: &mut SwapLoopState,
    amount_specified: U256,
    amount_remaining: &mut U256,
    amount_out_accum: &mut U256,
    crossings: Option<&mut Vec<TickCrossInfo>>,
    effective_swap_fee: u32,
    params: SwapParams,
    tick_next: TickIndex,
    tick_initialized: bool,
    sqrt_boundary: SqrtPriceX96,
    sqrt_target: SqrtPriceX96,
    #[cfg(feature = "protocol-fee")] total_protocol_fee: &mut U256,
) -> Result<bool, SwapSimError> {
    // If price limit is before the tick boundary, the cached boundary-cross amount is invalid.
    if sqrt_target != sqrt_boundary {
        return Ok(false);
    }

    let Some(liquidity) = NonZeroLiquidity::new(result.liquidity.value()) else {
        return Ok(false);
    };

    let Some(entry) = cache.get_or_compute_boundary_cross(
        tick_next,
        params.zero_for_one,
        result.sqrt_price_x96,
        sqrt_boundary,
        liquidity,
        effective_swap_fee,
    )?
    else {
        return Ok(false);
    };

    if !entry.can_cross_exact_in(*amount_remaining) {
        return Ok(false);
    }

    *amount_remaining = checked_sub_u256(*amount_remaining, entry.gross_in_to_cross)?;
    *amount_out_accum = checked_add_u256(*amount_out_accum, entry.amount_out_to_cross)?;
    result.sqrt_price_x96 = entry.sqrt_boundary_x96;

    #[allow(unused_mut)]
    let mut step_fee_amount = entry.fee_amount_to_cross;
    #[cfg(feature = "protocol-fee")]
    {
        deduct_protocol_fee_from_step(
            entry.amount_in_to_cross,
            &mut step_fee_amount,
            effective_swap_fee,
            params.protocol_fee,
            total_protocol_fee,
        )?;
    }
    accrue_lp_fee(result, step_fee_amount, params.zero_for_one)?;

    if tick_initialized {
        let liquidity_net_raw = ticks.cross_tick(tick_next)?;
        let liquidity_net = if params.zero_for_one {
            liquidity_net_raw
                .checked_neg()
                .ok_or(SwapSimError::AmountOverflow)?
        } else {
            liquidity_net_raw
        };

        let liquidity_after = add_liquidity_delta(result.liquidity, liquidity_net)?;
        result.liquidity = liquidity_after;

        if let Some(crossings) = crossings {
            crossings.push(TickCrossInfo {
                cumulative_input: checked_sub_u256(amount_specified, *amount_remaining)?,
                tick: tick_next,
                liquidity_after,
                fee_growth_global0_x128: result.fee_growth_global0_x128,
                fee_growth_global1_x128: result.fee_growth_global1_x128,
            });
        }
    }

    result.tick = tick_after_cross(tick_next, params.zero_for_one)?;

    Ok(true)
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn try_apply_cached_exact_out_cross<G: TickCrossing>(
    ticks: &G,
    cache: &PoolCache,
    result: &mut SwapLoopState,
    amount_remaining: &mut U256,
    amount_in_accum: &mut U256,
    crossings: Option<&mut Vec<TickCrossInfo>>,
    effective_swap_fee: u32,
    params: SwapParams,
    tick_next: TickIndex,
    tick_initialized: bool,
    sqrt_boundary: SqrtPriceX96,
    sqrt_target: SqrtPriceX96,
    #[cfg(feature = "protocol-fee")] total_protocol_fee: &mut U256,
) -> Result<bool, SwapSimError> {
    // If price limit is before the tick boundary, the cached boundary-cross amount is invalid.
    if sqrt_target != sqrt_boundary {
        return Ok(false);
    }

    let Some(liquidity) = NonZeroLiquidity::new(result.liquidity.value()) else {
        return Ok(false);
    };

    let Some(entry) = cache.get_or_compute_boundary_cross(
        tick_next,
        params.zero_for_one,
        result.sqrt_price_x96,
        sqrt_boundary,
        liquidity,
        effective_swap_fee,
    )?
    else {
        return Ok(false);
    };

    if !entry.can_cross_exact_out(*amount_remaining) {
        return Ok(false);
    }

    *amount_remaining = checked_sub_u256(*amount_remaining, entry.amount_out_to_cross)?;
    *amount_in_accum = checked_add_u256(*amount_in_accum, entry.gross_in_to_cross)?;
    result.sqrt_price_x96 = entry.sqrt_boundary_x96;

    #[allow(unused_mut)]
    let mut step_fee_amount = entry.fee_amount_to_cross;
    #[cfg(feature = "protocol-fee")]
    {
        deduct_protocol_fee_from_step(
            entry.amount_in_to_cross,
            &mut step_fee_amount,
            effective_swap_fee,
            params.protocol_fee,
            total_protocol_fee,
        )?;
    }
    accrue_lp_fee(result, step_fee_amount, params.zero_for_one)?;

    if tick_initialized {
        let liquidity_net_raw = ticks.cross_tick(tick_next)?;
        let liquidity_net = if params.zero_for_one {
            liquidity_net_raw
                .checked_neg()
                .ok_or(SwapSimError::AmountOverflow)?
        } else {
            liquidity_net_raw
        };

        let liquidity_after = add_liquidity_delta(result.liquidity, liquidity_net)?;
        result.liquidity = liquidity_after;

        if let Some(crossings) = crossings {
            crossings.push(TickCrossInfo {
                cumulative_input: U256::ZERO,
                tick: tick_next,
                liquidity_after,
                fee_growth_global0_x128: result.fee_growth_global0_x128,
                fee_growth_global1_x128: result.fee_growth_global1_x128,
            });
        }
    }

    result.tick = tick_after_cross(tick_next, params.zero_for_one)?;

    Ok(true)
}

/// Exact-input loop used by the default lightweight simulator.
///
/// This is intentionally separate from the full loop. It returns only the
/// lightweight result, records no crossings, and calls exact-in step math
/// directly instead of the generic signed `compute_swap_step` dispatcher.
fn execute_exact_in_light_loop<G: TickCrossing>(
    state: &PoolState,
    ticks: &G,
    cache: &PoolCache,
    effective_swap_fee: u32,
    tick_spacing: TickSpacing,
    params: SwapParams,
) -> Result<SwapSimulationResult, SwapSimError> {
    let mut result = SwapLoopState {
        sqrt_price_x96: state.sqrt_price_x96,
        tick: state.tick,
        liquidity: state.liquidity,
        fee_growth_global0_x128: state.fee_growth_global0_x128,
        fee_growth_global1_x128: state.fee_growth_global1_x128,
    };

    let amount_specified = params.amount.abs();
    let mut amount_remaining = amount_specified;
    let mut amount_out_accum = U256::ZERO;

    #[cfg(feature = "protocol-fee")]
    let mut total_protocol_fee = U256::ZERO;

    let mut iterations = 0usize;

    while !amount_remaining.is_zero() && result.sqrt_price_x96 != params.sqrt_price_limit_x96 {
        if iterations >= MAX_SWAP_ITERATIONS {
            return Err(SwapSimError::IterationLimitExceeded);
        }
        iterations += 1;

        let sqrt_start = result.sqrt_price_x96;
        let next_tick = ticks.next_initialized_tick_within_one_word(
            result.tick,
            tick_spacing,
            params.zero_for_one,
        );

        // V4 clamps tickNext before pricing because the bitmap is not aware
        // of global min/max tick bounds.
        let tick_next = next_tick.tick_next.clamp(TickIndex::MIN, TickIndex::MAX);

        let sqrt_boundary = cache.get_sqrt_price_at_tick(tick_next)?;
        let sqrt_target = get_sqrt_price_target(
            params.zero_for_one,
            sqrt_boundary,
            params.sqrt_price_limit_x96,
        );

        if try_apply_cached_exact_in_cross(
            ticks,
            cache,
            &mut result,
            amount_specified,
            &mut amount_remaining,
            &mut amount_out_accum,
            None,
            effective_swap_fee,
            params,
            tick_next,
            next_tick.initialized,
            sqrt_boundary,
            sqrt_target,
            #[cfg(feature = "protocol-fee")]
            &mut total_protocol_fee,
        )? {
            continue;
        }

        let computed = compute_swap_step_exact_in_fast(
            result.sqrt_price_x96,
            sqrt_target,
            result.liquidity,
            amount_remaining,
            effective_swap_fee,
            params.zero_for_one,
        )?;

        result.sqrt_price_x96 = computed.sqrt_ratio_next_x96;
        #[allow(unused_mut)]
        let mut step_fee_amount = computed.fee_amount;

        // V4 charges the caller the full swap fee for amount accounting first.
        // The protocol share is deducted only from the LP fee-growth amount after this.
        let in_plus_fee = checked_add_u256(computed.amount_in, step_fee_amount)?;
        amount_remaining = checked_sub_u256(amount_remaining, in_plus_fee)?;
        amount_out_accum = checked_add_u256(amount_out_accum, computed.amount_out)?;

        #[cfg(feature = "protocol-fee")]
        deduct_protocol_fee_from_step(
            computed.amount_in,
            &mut step_fee_amount,
            effective_swap_fee,
            params.protocol_fee,
            &mut total_protocol_fee,
        )?;
        accrue_lp_fee(&mut result, step_fee_amount, params.zero_for_one)?;

        if result.sqrt_price_x96 == sqrt_boundary {
            if next_tick.initialized {
                let liquidity_net_raw = ticks.cross_tick(tick_next)?;
                let liquidity_net = if params.zero_for_one {
                    liquidity_net_raw
                        .checked_neg()
                        .ok_or(SwapSimError::AmountOverflow)?
                } else {
                    liquidity_net_raw
                };

                result.liquidity = add_liquidity_delta(result.liquidity, liquidity_net)?;
            }

            result.tick = tick_after_cross(tick_next, params.zero_for_one)?;
        } else if result.sqrt_price_x96 != sqrt_start {
            // A non-boundary price movement is terminal for this step: either
            // the specified amount was exhausted or the price limit was hit.
            result.tick = get_tick_at_sqrt_price(&result.sqrt_price_x96);
            break;
        }
    }

    let input_received = checked_sub_u256(amount_specified, amount_remaining)?;
    let delta = make_delta_from_exact_in(params.zero_for_one, input_received, amount_out_accum)?;

    Ok(SwapSimulationResult {
        sqrt_price_x96: result.sqrt_price_x96,
        tick: result.tick,
        liquidity: result.liquidity,
        fee_growth_global0_x128: result.fee_growth_global0_x128,
        fee_growth_global1_x128: result.fee_growth_global1_x128,
        delta,
        #[cfg(feature = "protocol-fee")]
        protocol_fee_amount: {
            if total_protocol_fee > U256::from(u128::MAX) {
                return Err(SwapSimError::AmountOverflow);
            }
            total_protocol_fee.to::<u128>()
        },
    })
}

/// Exact-output loop used by the default lightweight simulator.
fn execute_exact_out_light_loop<G: TickCrossing>(
    state: &PoolState,
    ticks: &G,
    cache: &PoolCache,
    effective_swap_fee: u32,
    tick_spacing: TickSpacing,
    params: SwapParams,
) -> Result<SwapSimulationResult, SwapSimError> {
    let mut result = SwapLoopState {
        sqrt_price_x96: state.sqrt_price_x96,
        tick: state.tick,
        liquidity: state.liquidity,
        fee_growth_global0_x128: state.fee_growth_global0_x128,
        fee_growth_global1_x128: state.fee_growth_global1_x128,
    };

    let amount_specified = params.amount.abs();
    let mut amount_remaining = amount_specified;
    let mut amount_in_accum = U256::ZERO;

    #[cfg(feature = "protocol-fee")]
    let mut total_protocol_fee = U256::ZERO;

    let mut iterations = 0usize;

    while !amount_remaining.is_zero() && result.sqrt_price_x96 != params.sqrt_price_limit_x96 {
        if iterations >= MAX_SWAP_ITERATIONS {
            return Err(SwapSimError::IterationLimitExceeded);
        }
        iterations += 1;

        let sqrt_start = result.sqrt_price_x96;
        let next_tick = ticks.next_initialized_tick_within_one_word(
            result.tick,
            tick_spacing,
            params.zero_for_one,
        );

        // V4 clamps tickNext before pricing because the bitmap is not aware
        // of global min/max tick bounds.
        let tick_next = next_tick.tick_next.clamp(TickIndex::MIN, TickIndex::MAX);

        let sqrt_boundary = cache.get_sqrt_price_at_tick(tick_next)?;
        let sqrt_target = get_sqrt_price_target(
            params.zero_for_one,
            sqrt_boundary,
            params.sqrt_price_limit_x96,
        );

        if try_apply_cached_exact_out_cross(
            ticks,
            cache,
            &mut result,
            &mut amount_remaining,
            &mut amount_in_accum,
            None,
            effective_swap_fee,
            params,
            tick_next,
            next_tick.initialized,
            sqrt_boundary,
            sqrt_target,
            #[cfg(feature = "protocol-fee")]
            &mut total_protocol_fee,
        )? {
            continue;
        }

        let computed = compute_swap_step_exact_out_fast(
            result.sqrt_price_x96,
            sqrt_target,
            result.liquidity,
            amount_remaining,
            effective_swap_fee,
            params.zero_for_one,
        )?;

        result.sqrt_price_x96 = computed.sqrt_ratio_next_x96;
        #[allow(unused_mut)]
        let mut step_fee_amount = computed.fee_amount;

        // V4 charges the caller the full swap fee for amount accounting first.
        // The protocol share is deducted only from the LP fee-growth amount after this.
        let in_plus_fee = checked_add_u256(computed.amount_in, step_fee_amount)?;
        amount_remaining = checked_sub_u256(amount_remaining, computed.amount_out)?;
        amount_in_accum = checked_add_u256(amount_in_accum, in_plus_fee)?;

        #[cfg(feature = "protocol-fee")]
        deduct_protocol_fee_from_step(
            computed.amount_in,
            &mut step_fee_amount,
            effective_swap_fee,
            params.protocol_fee,
            &mut total_protocol_fee,
        )?;
        accrue_lp_fee(&mut result, step_fee_amount, params.zero_for_one)?;

        if result.sqrt_price_x96 == sqrt_boundary {
            if next_tick.initialized {
                let liquidity_net_raw = ticks.cross_tick(tick_next)?;
                let liquidity_net = if params.zero_for_one {
                    liquidity_net_raw
                        .checked_neg()
                        .ok_or(SwapSimError::AmountOverflow)?
                } else {
                    liquidity_net_raw
                };

                result.liquidity = add_liquidity_delta(result.liquidity, liquidity_net)?;
            }

            result.tick = tick_after_cross(tick_next, params.zero_for_one)?;
        } else if result.sqrt_price_x96 != sqrt_start {
            // A non-boundary price movement is terminal for this step: either
            // the specified amount was exhausted or the price limit was hit.
            result.tick = get_tick_at_sqrt_price(&result.sqrt_price_x96);
            break;
        }
    }

    let output_sent = checked_sub_u256(amount_specified, amount_remaining)?;
    let delta = make_delta_from_exact_out(params.zero_for_one, amount_in_accum, output_sent)?;

    Ok(SwapSimulationResult {
        sqrt_price_x96: result.sqrt_price_x96,
        tick: result.tick,
        liquidity: result.liquidity,
        fee_growth_global0_x128: result.fee_growth_global0_x128,
        fee_growth_global1_x128: result.fee_growth_global1_x128,
        delta,
        #[cfg(feature = "protocol-fee")]
        protocol_fee_amount: {
            if total_protocol_fee > U256::from(u128::MAX) {
                return Err(SwapSimError::AmountOverflow);
            }
            total_protocol_fee.to::<u128>()
        },
    })
}

fn execute_exact_in_loop<G: TickCrossing, const RECORD_CROSSINGS: bool>(
    state: &PoolState,
    ticks: &G,
    cache: &PoolCache,
    effective_swap_fee: u32,
    tick_spacing: TickSpacing,
    params: SwapParams,
) -> Result<(SwapSimulationResult, Option<Vec<TickCrossInfo>>), SwapSimError> {
    let mut result = SwapLoopState {
        sqrt_price_x96: state.sqrt_price_x96,
        tick: state.tick,
        liquidity: state.liquidity,
        fee_growth_global0_x128: state.fee_growth_global0_x128,
        fee_growth_global1_x128: state.fee_growth_global1_x128,
    };

    let amount_specified = params.amount.abs();
    let mut amount_remaining = amount_specified;
    let mut amount_out_accum = U256::ZERO;

    let mut crossings = if RECORD_CROSSINGS {
        Some(Vec::<TickCrossInfo>::new())
    } else {
        None
    };

    #[cfg(feature = "protocol-fee")]
    let mut total_protocol_fee = U256::ZERO;

    let mut iterations = 0usize;

    while !amount_remaining.is_zero() && result.sqrt_price_x96 != params.sqrt_price_limit_x96 {
        if iterations >= MAX_SWAP_ITERATIONS {
            return Err(SwapSimError::IterationLimitExceeded);
        }
        iterations += 1;

        let sqrt_start = result.sqrt_price_x96;
        let next_tick = ticks.next_initialized_tick_within_one_word(
            result.tick,
            tick_spacing,
            params.zero_for_one,
        );

        // V4 clamps tickNext before pricing because the bitmap is not aware
        // of global min/max tick bounds.
        let tick_next = next_tick.tick_next.clamp(TickIndex::MIN, TickIndex::MAX);

        let sqrt_boundary = cache.get_sqrt_price_at_tick(tick_next)?;
        let sqrt_target = get_sqrt_price_target(
            params.zero_for_one,
            sqrt_boundary,
            params.sqrt_price_limit_x96,
        );

        if try_apply_cached_exact_in_cross(
            ticks,
            cache,
            &mut result,
            amount_specified,
            &mut amount_remaining,
            &mut amount_out_accum,
            crossings.as_mut(),
            effective_swap_fee,
            params,
            tick_next,
            next_tick.initialized,
            sqrt_boundary,
            sqrt_target,
            #[cfg(feature = "protocol-fee")]
            &mut total_protocol_fee,
        )? {
            continue;
        }

        let computed = compute_swap_step_exact_in_fast(
            result.sqrt_price_x96,
            sqrt_target,
            result.liquidity,
            amount_remaining,
            effective_swap_fee,
            params.zero_for_one,
        )?;

        result.sqrt_price_x96 = computed.sqrt_ratio_next_x96;
        #[allow(unused_mut)]
        let mut step_fee_amount = computed.fee_amount;

        // V4 charges the caller the full swap fee for amount accounting first.
        // The protocol share is deducted only from the LP fee-growth amount after this.
        let in_plus_fee = checked_add_u256(computed.amount_in, step_fee_amount)?;
        amount_remaining = checked_sub_u256(amount_remaining, in_plus_fee)?;
        amount_out_accum = checked_add_u256(amount_out_accum, computed.amount_out)?;

        #[cfg(feature = "protocol-fee")]
        deduct_protocol_fee_from_step(
            computed.amount_in,
            &mut step_fee_amount,
            effective_swap_fee,
            params.protocol_fee,
            &mut total_protocol_fee,
        )?;
        accrue_lp_fee(&mut result, step_fee_amount, params.zero_for_one)?;

        if result.sqrt_price_x96 == sqrt_boundary {
            if next_tick.initialized {
                let liquidity_net_raw = ticks.cross_tick(tick_next)?;
                let liquidity_net = if params.zero_for_one {
                    liquidity_net_raw
                        .checked_neg()
                        .ok_or(SwapSimError::AmountOverflow)?
                } else {
                    liquidity_net_raw
                };

                let liquidity_after = add_liquidity_delta(result.liquidity, liquidity_net)?;
                result.liquidity = liquidity_after;

                if RECORD_CROSSINGS && let Some(crossings) = crossings.as_mut() {
                    crossings.push(TickCrossInfo {
                        cumulative_input: checked_sub_u256(amount_specified, amount_remaining)?,
                        tick: tick_next,
                        liquidity_after,
                        fee_growth_global0_x128: result.fee_growth_global0_x128,
                        fee_growth_global1_x128: result.fee_growth_global1_x128,
                    });
                }
            }

            result.tick = tick_after_cross(tick_next, params.zero_for_one)?;
        } else if result.sqrt_price_x96 != sqrt_start {
            if amount_remaining.is_zero() || result.sqrt_price_x96 == params.sqrt_price_limit_x96 {
                result.tick = get_tick_at_sqrt_price(&result.sqrt_price_x96);
                break;
            }

            result.tick = get_tick_at_sqrt_price(&result.sqrt_price_x96);
        }
    }

    let input_received = checked_sub_u256(amount_specified, amount_remaining)?;
    let delta = make_delta_from_exact_in(params.zero_for_one, input_received, amount_out_accum)?;

    let light = SwapSimulationResult {
        sqrt_price_x96: result.sqrt_price_x96,
        tick: result.tick,
        liquidity: result.liquidity,
        fee_growth_global0_x128: result.fee_growth_global0_x128,
        fee_growth_global1_x128: result.fee_growth_global1_x128,
        delta,
        #[cfg(feature = "protocol-fee")]
        protocol_fee_amount: {
            if total_protocol_fee > U256::from(u128::MAX) {
                return Err(SwapSimError::AmountOverflow);
            }
            total_protocol_fee.to::<u128>()
        },
    };

    Ok((light, crossings))
}

fn execute_exact_out_loop<G: TickCrossing, const RECORD_CROSSINGS: bool>(
    state: &PoolState,
    ticks: &G,
    cache: &PoolCache,
    effective_swap_fee: u32,
    tick_spacing: TickSpacing,
    params: SwapParams,
) -> Result<(SwapSimulationResult, Option<Vec<TickCrossInfo>>), SwapSimError> {
    let mut result = SwapLoopState {
        sqrt_price_x96: state.sqrt_price_x96,
        tick: state.tick,
        liquidity: state.liquidity,
        fee_growth_global0_x128: state.fee_growth_global0_x128,
        fee_growth_global1_x128: state.fee_growth_global1_x128,
    };

    let amount_specified = params.amount.abs();
    let mut amount_remaining = amount_specified;
    let mut amount_in_accum = U256::ZERO;

    let mut crossings = if RECORD_CROSSINGS {
        Some(Vec::<TickCrossInfo>::new())
    } else {
        None
    };

    #[cfg(feature = "protocol-fee")]
    let mut total_protocol_fee = U256::ZERO;

    let mut iterations = 0usize;

    while !amount_remaining.is_zero() && result.sqrt_price_x96 != params.sqrt_price_limit_x96 {
        if iterations >= MAX_SWAP_ITERATIONS {
            return Err(SwapSimError::IterationLimitExceeded);
        }
        iterations += 1;

        let sqrt_start = result.sqrt_price_x96;
        let next_tick = ticks.next_initialized_tick_within_one_word(
            result.tick,
            tick_spacing,
            params.zero_for_one,
        );

        // V4 clamps tickNext before pricing because the bitmap is not aware
        // of global min/max tick bounds.
        let tick_next = next_tick.tick_next.clamp(TickIndex::MIN, TickIndex::MAX);

        let sqrt_boundary = cache.get_sqrt_price_at_tick(tick_next)?;
        let sqrt_target = get_sqrt_price_target(
            params.zero_for_one,
            sqrt_boundary,
            params.sqrt_price_limit_x96,
        );

        if try_apply_cached_exact_out_cross(
            ticks,
            cache,
            &mut result,
            &mut amount_remaining,
            &mut amount_in_accum,
            crossings.as_mut(),
            effective_swap_fee,
            params,
            tick_next,
            next_tick.initialized,
            sqrt_boundary,
            sqrt_target,
            #[cfg(feature = "protocol-fee")]
            &mut total_protocol_fee,
        )? {
            continue;
        }

        let computed = compute_swap_step_exact_out_fast(
            result.sqrt_price_x96,
            sqrt_target,
            result.liquidity,
            amount_remaining,
            effective_swap_fee,
            params.zero_for_one,
        )?;

        result.sqrt_price_x96 = computed.sqrt_ratio_next_x96;
        #[allow(unused_mut)]
        let mut step_fee_amount = computed.fee_amount;

        // V4 charges the caller the full swap fee for amount accounting first.
        // The protocol share is deducted only from the LP fee-growth amount after this.
        let in_plus_fee = checked_add_u256(computed.amount_in, step_fee_amount)?;
        amount_remaining = checked_sub_u256(amount_remaining, computed.amount_out)?;
        amount_in_accum = checked_add_u256(amount_in_accum, in_plus_fee)?;

        #[cfg(feature = "protocol-fee")]
        deduct_protocol_fee_from_step(
            computed.amount_in,
            &mut step_fee_amount,
            effective_swap_fee,
            params.protocol_fee,
            &mut total_protocol_fee,
        )?;
        accrue_lp_fee(&mut result, step_fee_amount, params.zero_for_one)?;

        if result.sqrt_price_x96 == sqrt_boundary {
            if next_tick.initialized {
                let liquidity_net_raw = ticks.cross_tick(tick_next)?;
                let liquidity_net = if params.zero_for_one {
                    liquidity_net_raw
                        .checked_neg()
                        .ok_or(SwapSimError::AmountOverflow)?
                } else {
                    liquidity_net_raw
                };

                let liquidity_after = add_liquidity_delta(result.liquidity, liquidity_net)?;
                result.liquidity = liquidity_after;

                if RECORD_CROSSINGS && let Some(crossings) = crossings.as_mut() {
                    crossings.push(TickCrossInfo {
                        cumulative_input: U256::ZERO,
                        tick: tick_next,
                        liquidity_after,
                        fee_growth_global0_x128: result.fee_growth_global0_x128,
                        fee_growth_global1_x128: result.fee_growth_global1_x128,
                    });
                }
            }

            result.tick = tick_after_cross(tick_next, params.zero_for_one)?;
        } else if result.sqrt_price_x96 != sqrt_start {
            if amount_remaining.is_zero() || result.sqrt_price_x96 == params.sqrt_price_limit_x96 {
                result.tick = get_tick_at_sqrt_price(&result.sqrt_price_x96);
                break;
            }

            result.tick = get_tick_at_sqrt_price(&result.sqrt_price_x96);
        }
    }

    let output_sent = checked_sub_u256(amount_specified, amount_remaining)?;
    let delta = make_delta_from_exact_out(params.zero_for_one, amount_in_accum, output_sent)?;

    let light = SwapSimulationResult {
        sqrt_price_x96: result.sqrt_price_x96,
        tick: result.tick,
        liquidity: result.liquidity,
        fee_growth_global0_x128: result.fee_growth_global0_x128,
        fee_growth_global1_x128: result.fee_growth_global1_x128,
        delta,
        #[cfg(feature = "protocol-fee")]
        protocol_fee_amount: {
            if total_protocol_fee > U256::from(u128::MAX) {
                return Err(SwapSimError::AmountOverflow);
            }
            total_protocol_fee.to::<u128>()
        },
    };

    Ok((light, crossings))
}

/// Returns the loosest valid price limit for a given swap direction.
///
/// Matches V4: zeroForOne must stay strictly above `MIN_SQRT_PRICE`;
/// oneForZero must stay strictly below `MAX_SQRT_PRICE`.
#[inline]
fn extreme_price_limit(zero_for_one: bool) -> SqrtPriceX96 {
    if zero_for_one {
        SqrtPriceX96::from_u256(SqrtPriceX96::MIN.as_u256() + U256::ONE)
            .expect("MIN_SQRT_PRICE + 1 is a valid price")
    } else {
        SqrtPriceX96::from_u256(SqrtPriceX96::MAX.as_u256() - U256::ONE)
            .expect("MAX_SQRT_PRICE - 1 is a valid price")
    }
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

#[inline]
fn validate_tick_range(_tick: TickIndex) -> Result<(), SwapSimError> {
    Ok(())
}

trait FeeGrowthTicks {
    fn fee_tick(&self, tick: TickIndex) -> Option<TickInfo>;
}

impl FeeGrowthTicks for PoolTicksReadGuard<'_> {
    fn fee_tick(&self, tick: TickIndex) -> Option<TickInfo> {
        self.get(tick)
    }
}

impl FeeGrowthTicks for PoolTicksWriteGuard<'_> {
    fn fee_tick(&self, tick: TickIndex) -> Option<TickInfo> {
        self.get(tick)
    }
}

#[inline]
fn fee_growth_inside_from_ticks<T: FeeGrowthTicks>(
    ticks: &T,
    tick_current: TickIndex,
    tick_lower: TickIndex,
    tick_upper: TickIndex,
    fee_growth_global0_x128: U256,
    fee_growth_global1_x128: U256,
) -> (U256, U256) {
    let lower = ticks.fee_tick(tick_lower).unwrap_or_default();
    let upper = ticks.fee_tick(tick_upper).unwrap_or_default();

    if tick_current < tick_lower {
        (
            lower
                .fee_growth_outside0_x128
                .wrapping_sub(upper.fee_growth_outside0_x128),
            lower
                .fee_growth_outside1_x128
                .wrapping_sub(upper.fee_growth_outside1_x128),
        )
    } else if tick_current >= tick_upper {
        (
            upper
                .fee_growth_outside0_x128
                .wrapping_sub(lower.fee_growth_outside0_x128),
            upper
                .fee_growth_outside1_x128
                .wrapping_sub(lower.fee_growth_outside1_x128),
        )
    } else {
        (
            fee_growth_global0_x128
                .wrapping_sub(lower.fee_growth_outside0_x128)
                .wrapping_sub(upper.fee_growth_outside0_x128),
            fee_growth_global1_x128
                .wrapping_sub(lower.fee_growth_outside1_x128)
                .wrapping_sub(upper.fee_growth_outside1_x128),
        )
    }
}

fn liquidity_principal_delta(
    state: &PoolState,
    cache: &PoolCache,
    params: ModifyLiquidityParams,
) -> Result<BalanceDelta, SwapSimError> {
    if params.liquidity_delta == 0 {
        return Ok(BalanceDelta::default());
    }

    let sqrt_price_lower_x96 = cache.get_sqrt_price_at_tick(params.tick_lower)?;
    let sqrt_price_upper_x96 = cache.get_sqrt_price_at_tick(params.tick_upper)?;
    let mut delta = BalanceDelta::default();

    if state.tick < params.tick_lower {
        delta.amount0 = get_amount0_delta_signed(
            sqrt_price_lower_x96,
            sqrt_price_upper_x96,
            params.liquidity_delta,
        )?;
    } else if state.tick < params.tick_upper {
        delta.amount0 = get_amount0_delta_signed(
            state.sqrt_price_x96,
            sqrt_price_upper_x96,
            params.liquidity_delta,
        )?;
        delta.amount1 = get_amount1_delta_signed(
            sqrt_price_lower_x96,
            state.sqrt_price_x96,
            params.liquidity_delta,
        )?;
    } else {
        delta.amount1 = get_amount1_delta_signed(
            sqrt_price_lower_x96,
            sqrt_price_upper_x96,
            params.liquidity_delta,
        )?;
    }
    Ok(delta)
}

/// Unsigned token0 delta for a liquidity change between two sqrt prices.
///
/// This delegates to the optimized `sqrt_price_math` implementation so
/// liquidity modifications and swap steps share the same fast, audited math.
fn get_amount0_delta_unsigned(
    sqrt_price_a_x96: SqrtPriceX96,
    sqrt_price_b_x96: SqrtPriceX96,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SwapSimError> {
    let Some(liquidity) = NonZeroLiquidity::new(liquidity) else {
        return Ok(U256::ZERO);
    };

    get_amount0_delta(sqrt_price_a_x96, sqrt_price_b_x96, liquidity, round_up)
        .map(|(amount, _)| amount)
        .map_err(|_| SwapSimError::AmountOverflow)
}

/// Unsigned token1 delta for a liquidity change between two sqrt prices.
///
/// This delegates to the optimized `sqrt_price_math` implementation so
/// liquidity modifications and swap steps share the same fast, audited math.
fn get_amount1_delta_unsigned(
    sqrt_price_a_x96: SqrtPriceX96,
    sqrt_price_b_x96: SqrtPriceX96,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SwapSimError> {
    let Some(liquidity) = NonZeroLiquidity::new(liquidity) else {
        return Ok(U256::ZERO);
    };

    get_amount1_delta(sqrt_price_a_x96, sqrt_price_b_x96, liquidity, round_up)
        .map(|(amount, _)| amount)
        .map_err(|_| SwapSimError::AmountOverflow)
}

/// Convert a U256 magnitude and sign flag to `i128`, accepting `i128::MIN`.
#[inline]
fn u256_magnitude_to_i128(amount: U256, negative: bool) -> Result<i128, SwapSimError> {
    if negative {
        // i128::MIN = −2^127; magnitude 2^127 is the only valid negative that
        // cannot be represented as a positive i128.
        let max_neg_magnitude = U256::from(1u128) << 127u32;
        if amount > max_neg_magnitude {
            return Err(SwapSimError::AmountOverflow);
        }
        if amount == max_neg_magnitude {
            return Ok(i128::MIN);
        }
        Ok(-(amount.to::<u128>() as i128))
    } else {
        if amount > U256::from(i128::MAX as u128) {
            return Err(SwapSimError::AmountOverflow);
        }
        Ok(amount.to::<u128>() as i128)
    }
}

fn get_amount0_delta_signed(
    sqrt_price_a_x96: SqrtPriceX96,
    sqrt_price_b_x96: SqrtPriceX96,
    liquidity_delta: i128,
) -> Result<i128, SwapSimError> {
    if liquidity_delta < 0 {
        let amount = get_amount0_delta_unsigned(
            sqrt_price_a_x96,
            sqrt_price_b_x96,
            liquidity_delta.unsigned_abs(),
            false,
        )?;
        u256_magnitude_to_i128(amount, false)
    } else {
        let amount = get_amount0_delta_unsigned(
            sqrt_price_a_x96,
            sqrt_price_b_x96,
            liquidity_delta as u128,
            true,
        )?;
        u256_magnitude_to_i128(amount, true)
    }
}

fn get_amount1_delta_signed(
    sqrt_price_a_x96: SqrtPriceX96,
    sqrt_price_b_x96: SqrtPriceX96,
    liquidity_delta: i128,
) -> Result<i128, SwapSimError> {
    if liquidity_delta < 0 {
        let amount = get_amount1_delta_unsigned(
            sqrt_price_a_x96,
            sqrt_price_b_x96,
            liquidity_delta.unsigned_abs(),
            false,
        )?;
        u256_magnitude_to_i128(amount, false)
    } else {
        let amount = get_amount1_delta_unsigned(
            sqrt_price_a_x96,
            sqrt_price_b_x96,
            liquidity_delta as u128,
            true,
        )?;
        u256_magnitude_to_i128(amount, true)
    }
}

/// Apply a signed `liquidity_net` delta to the active liquidity.
#[inline]
fn add_liquidity_delta(liquidity: Liquidity, delta: i128) -> Result<Liquidity, SwapSimError> {
    let raw = if delta < 0 {
        liquidity
            .value()
            .checked_sub(delta.unsigned_abs())
            .ok_or(SwapSimError::LiquidityUnderflow)?
    } else {
        liquidity
            .value()
            .checked_add(delta as u128)
            .ok_or(SwapSimError::LiquidityOverflow)?
    };

    Ok(Liquidity::new(raw))
}

#[inline]
fn tick_after_cross(tick_next: TickIndex, zero_for_one: bool) -> Result<TickIndex, SwapSimError> {
    if zero_for_one {
        tick_next
            .value()
            .checked_sub(1)
            .and_then(TickIndex::new)
            .ok_or(SwapSimError::InvalidTick)
    } else {
        Ok(tick_next)
    }
}

/// Convert a [`SignedAmount`] to `i128`, correctly handling `i128::MIN`.
///
/// Positive values must fit within `i128::MAX`. Negative values may use
/// magnitude `2^127`, which maps to `i128::MIN`.
#[inline]
#[cfg(test)]
fn signed_amount_to_i128(amount: SignedAmount) -> Result<i128, SwapSimError> {
    let abs = amount.abs();

    if amount.is_negative() {
        let max_neg_magnitude = U256::from(1u128) << 127u32; // 2^127
        if abs > max_neg_magnitude {
            return Err(SwapSimError::AmountOverflow);
        }
        if abs == max_neg_magnitude {
            return Ok(i128::MIN);
        }
        Ok(-(abs.to::<u128>() as i128))
    } else {
        if abs > U256::from(i128::MAX as u128) {
            return Err(SwapSimError::AmountOverflow);
        }
        Ok(abs.to::<u128>() as i128)
    }
}

#[cfg(feature = "protocol-fee")]
fn calculate_swap_fee(protocol_fee: Option<u32>, lp_fee: u32) -> Result<u32, SwapSimError> {
    use protocol_fee_consts::{MAX_PROTOCOL_FEE, PIPS_DENOMINATOR};

    validate_fee(lp_fee)?;

    let Some(protocol_fee) = protocol_fee else {
        return Ok(lp_fee);
    };

    if protocol_fee > MAX_PROTOCOL_FEE {
        return Err(SwapSimError::FeeTooLarge);
    }
    if protocol_fee == 0 {
        return Ok(lp_fee);
    }

    // V4's `calculateSwapFee`:
    //   protocolFee + lpFee − floor(protocolFee × lpFee / PIPS_DENOMINATOR)
    let overlap = (u64::from(protocol_fee) * u64::from(lp_fee)) / u64::from(PIPS_DENOMINATOR);
    let combined = u64::from(protocol_fee) + u64::from(lp_fee) - overlap;

    if combined > u64::from(PIPS_DENOMINATOR) {
        return Err(SwapSimError::FeeTooLarge);
    }

    Ok(combined as u32)
}

#[inline]
fn validate_fee(fee: u32) -> Result<(), SwapSimError> {
    if fee > MAX_SWAP_FEE {
        Err(SwapSimError::FeeTooLarge)
    } else {
        Ok(())
    }
}

/// V4 allows 100 % fee for exact-input swaps but not for exact-output
/// (a 100 % fee would consume the entire input leaving nothing for output).
#[inline]
fn validate_swap_fee_for_exactness(fee: u32, exact_in: bool) -> Result<(), SwapSimError> {
    validate_fee(fee)?;
    if !exact_in && fee == MAX_SWAP_FEE {
        Err(SwapSimError::FeeTooLarge)
    } else {
        Ok(())
    }
}
/// Return the next initialized tick within the current bitmap word.
///
/// Emulates V4's `TickBitmap.nextInitializedTickWithinOneWord` using the
/// ordered `PoolTicks` BTreeMap. The word-boundary arithmetic ensures fee
/// rounding matches V4 exactly:
///
/// * **zeroForOne** (search downward): word start = `(compressed / 256) × 256`.
/// * **oneForZero** (search upward): word end = `((compressed+1) / 256) × 256 + 255`.
#[inline(always)]
fn next_initialized_tick_within_one_word_fallback<G: TickCrossing + ?Sized>(
    ticks: &G,
    tick: TickIndex,
    tick_spacing: TickSpacing,
    zero_for_one: bool,
) -> NextInitializedTick {
    let compressed = tick.div_euclid(tick_spacing.as_i32());

    let boundary_compressed = if zero_for_one {
        // Lowest compressed tick in the current 256-bit word.
        compressed - compressed.rem_euclid(256)
    } else {
        // Highest compressed tick in the word that contains (compressed + 1).
        let next_compressed = compressed + 1;
        next_compressed + (255 - next_compressed.rem_euclid(256))
    };

    let boundary_tick = TickIndex::new(
        boundary_compressed
            .saturating_mul(tick_spacing.as_i32())
            .clamp(TickIndex::MIN.value(), TickIndex::MAX.value()),
    )
    .expect("must be within tick index bounds");

    let next = ticks.next_initialized_tick(tick, zero_for_one);

    if zero_for_one {
        if next.initialized && next.tick_next >= boundary_tick {
            next
        } else {
            NextInitializedTick {
                tick_next: boundary_tick,
                initialized: false,
            }
        }
    } else if next.initialized && next.tick_next <= boundary_tick {
        next
    } else {
        NextInitializedTick {
            tick_next: boundary_tick,
            initialized: false,
        }
    }
}

/// Validate pool sqrt price and price limit against V4's revert conditions.
fn validate_price_limit(
    state: &PoolState,
    zero_for_one: bool,
    limit: SqrtPriceX96,
) -> Result<(), SwapSimError> {
    let min_price = SqrtPriceX96::MIN;
    let max_price = SqrtPriceX96::MAX;

    if !state.sqrt_price_x96.is_valid() {
        return Err(SwapSimError::InvalidPoolSqrtPrice);
    }

    if zero_for_one {
        // V4: limit must be strictly below current price and strictly above MIN_SQRT_PRICE.
        if limit >= state.sqrt_price_x96 {
            return Err(SwapSimError::PriceLimitAlreadyExceeded);
        }
        if limit <= min_price {
            return Err(SwapSimError::PriceLimitOutOfBounds);
        }
    } else {
        // V4: limit must be strictly above current price and strictly below MAX_SQRT_PRICE.
        if limit <= state.sqrt_price_x96 {
            return Err(SwapSimError::PriceLimitAlreadyExceeded);
        }
        if limit >= max_price {
            return Err(SwapSimError::PriceLimitOutOfBounds);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::core::{math::tick::get_sqrt_price_at_tick, types::tick::TickIndex};

    use super::*;
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

    fn tick_info(
        tick_idx: i32,
        liquidity_net: i128,
        liquidity_gross: u128,
    ) -> (TickIndex, TickInfo) {
        (
            tick(tick_idx),
            TickInfo::new(
                sqrt_at(tick_idx).value(),
                liquidity_gross,
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
        tick_spacing: TickSpacing,
        ticks: impl IntoIterator<Item = (TickIndex, TickInfo)>,
    ) -> Pool {
        let tick_map =
            PoolTicks::from_snapshot(tick_spacing, ticks.into_iter().collect::<BTreeMap<_, _>>())
                .expect("valid ticks");

        let cache = Arc::new(PoolCache::default());

        Pool {
            state: Arc::new(RwLock::new(PoolState {
                sqrt_price_x96,
                tick: self::tick(tick),
                liquidity: Liquidity::new(liquidity),
                fee_growth_global0_x128: U256::ZERO,
                fee_growth_global1_x128: U256::ZERO,
            })),
            fee,
            tick_spacing,
            ticks: tick_map,
            cache,
            price_cache: Arc::new(PriceCache::new(
                sqrt_price_x96_to_price(sqrt_price_x96),
                fee,
            )),
        }
    }

    fn read_pool_state(pool: &Pool) -> PoolState {
        *pool.state.read()
    }

    fn basic_pool(ticks: &[(TickIndex, TickInfo)]) -> Pool {
        make_pool(
            sqrt_price_1_1(),
            0,
            1_000_000_000_000_000_000u128,
            3_000,
            tick_spacing_60(),
            ticks.iter().copied(),
        )
    }

    fn make_params(
        zero_for_one: bool,
        amount: SignedAmount,
        sqrt_price_limit_x96: SqrtPriceX96,
    ) -> SwapParams {
        SwapParams {
            zero_for_one,
            amount,
            sqrt_price_limit_x96,
            #[cfg(feature = "protocol-fee")]
            protocol_fee: None,
        }
    }

    #[test]
    fn swap_zero_amount_does_nothing() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let before = read_pool_state(&pool);

        let result = pool
            .simulate_swap(make_params(
                true,
                SignedAmount::negative(U256::ZERO),
                sqrt_at(-120),
            ))
            .expect("zero-amount swap should succeed");

        assert_eq!(result.sqrt_price_x96, before.sqrt_price_x96);
        assert_eq!(result.tick, before.tick);
        assert_eq!(result.liquidity, before.liquidity);
        assert_eq!(result.delta, BalanceDelta::default());
    }

    #[test]
    fn swap_commits_state() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let before = read_pool_state(&pool);
        let limit = sqrt_at(-120);

        let result = pool
            .swap(make_params(true, SignedAmount::negative(ether(1)), limit))
            .expect("swap should succeed");

        let after = read_pool_state(&pool);
        assert_ne!(after.sqrt_price_x96, before.sqrt_price_x96);
        assert_eq!(after.sqrt_price_x96, result.sqrt_price_x96);
        assert_eq!(after.tick, result.tick);
        assert_eq!(after.liquidity, result.liquidity);
    }

    #[test]
    fn simulate_swap_does_not_change_pool_state() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let before = read_pool_state(&pool);
        let limit = sqrt_at(-120);

        let result = pool
            .simulate_swap(make_params(true, SignedAmount::negative(ether(1)), limit))
            .expect("simulation should succeed");

        let after = read_pool_state(&pool);
        assert_eq!(after.sqrt_price_x96, before.sqrt_price_x96);
        assert_eq!(after.tick, before.tick);
        assert_eq!(after.liquidity, before.liquidity);
        assert_ne!(result.sqrt_price_x96, before.sqrt_price_x96);
    }

    #[test]
    fn light_and_full_simulation_match_final_result() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let params = make_params(true, SignedAmount::negative(ether(1)), sqrt_at(-120));

        let light = pool
            .simulate_swap(params)
            .expect("light simulation should succeed");
        let full = pool
            .simulate_swap_full(params)
            .expect("full simulation should succeed");

        assert_eq!(light.sqrt_price_x96, full.sqrt_price_x96);
        assert_eq!(light.tick, full.tick);
        assert_eq!(light.liquidity, full.liquidity);
        assert_eq!(light.delta, full.delta);

        #[cfg(feature = "protocol-fee")]
        assert_eq!(light.protocol_fee_amount, full.protocol_fee_amount);
    }

    #[test]
    fn light_and_full_exact_output_match_final_result() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let params = make_params(
            true,
            SignedAmount::positive(U256::from(1_000_000_000_000u128)),
            sqrt_at(-120),
        );

        let light = pool
            .simulate_swap(params)
            .expect("light exact-output simulation should succeed");
        let full = pool
            .simulate_swap_full(params)
            .expect("full exact-output simulation should succeed");

        assert_eq!(light.sqrt_price_x96, full.sqrt_price_x96);
        assert_eq!(light.tick, full.tick);
        assert_eq!(light.liquidity, full.liquidity);
        assert_eq!(light.delta, full.delta);

        #[cfg(feature = "protocol-fee")]
        assert_eq!(light.protocol_fee_amount, full.protocol_fee_amount);
    }

    #[test]
    fn zero_for_one_exact_in_delta_signs() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let limit = sqrt_at(-120);

        let result = pool
            .simulate_swap(make_params(true, SignedAmount::negative(ether(1)), limit))
            .expect("swap should succeed");

        // caller / PoolManager accounting perspective:
        // caller owes token0, receives token1
        assert!(result.delta.amount0 < 0, "caller owes token0");
        assert!(result.delta.amount1 > 0, "caller receives token1");

        assert!(delta_amount_in(&result.delta, true) > 0);
        assert!(delta_amount_out(&result.delta, true) > 0);
    }

    #[test]
    fn one_for_zero_exact_in_delta_signs() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let limit = sqrt_at(120);

        let result = pool
            .simulate_swap(make_params(false, SignedAmount::negative(ether(1)), limit))
            .expect("swap should succeed");

        // caller / PoolManager accounting perspective:
        // caller receives token0, owes token1
        assert!(result.delta.amount1 < 0, "caller owes token1");
        assert!(result.delta.amount0 > 0, "caller receives token0");

        assert!(delta_amount_in(&result.delta, false) > 0);
        assert!(delta_amount_out(&result.delta, false) > 0);
    }
    #[test]
    fn zero_for_one_price_moves_down() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let limit = sqrt_at(-120);

        let before_price = read_pool_state(&pool).sqrt_price_x96;
        let result = pool
            .simulate_swap(make_params(true, SignedAmount::negative(ether(1)), limit))
            .unwrap();

        assert!(result.sqrt_price_x96 <= before_price, "price must not rise");
        assert!(result.sqrt_price_x96 >= limit, "price must not cross limit");
    }

    #[test]
    fn one_for_zero_price_moves_up() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let limit = sqrt_at(120);

        let before_price = read_pool_state(&pool).sqrt_price_x96;
        let result = pool
            .simulate_swap(make_params(false, SignedAmount::negative(ether(1)), limit))
            .unwrap();

        assert!(result.sqrt_price_x96 >= before_price, "price must not fall");
        assert!(result.sqrt_price_x96 <= limit, "price must not cross limit");
    }

    #[test]
    fn amount_in_and_out_are_consistent_with_raw_delta() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let limit = sqrt_at(-120);

        let result = pool
            .simulate_swap(make_params(true, SignedAmount::negative(ether(1)), limit))
            .unwrap();

        // For zeroForOne: input = token0 (amount0 < 0), output = token1 (amount1 > 0).
        assert_eq!(
            delta_amount_in(&result.delta, true),
            result.delta.amount0.unsigned_abs()
        );
        assert_eq!(
            delta_amount_out(&result.delta, true),
            result.delta.amount1.unsigned_abs()
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
    fn signed_amount_to_i128_accepts_i128_min() {
        let min_magnitude = U256::from(1u128) << 127u32;
        let amount = SignedAmount::negative(min_magnitude);
        let result = signed_amount_to_i128(amount).expect("i128::MIN must be accepted");
        assert_eq!(result, i128::MIN);
    }

    #[test]
    fn signed_amount_to_i128_rejects_overflow() {
        let too_large = (U256::from(1u128) << 127u32) + U256::ONE;
        assert_eq!(
            signed_amount_to_i128(SignedAmount::negative(too_large)),
            Err(SwapSimError::AmountOverflow)
        );
    }

    #[test]
    fn signed_amount_to_i128_accepts_i128_max() {
        let amount = SignedAmount::positive(U256::from(i128::MAX as u128));
        assert_eq!(signed_amount_to_i128(amount).unwrap(), i128::MAX);
    }

    #[test]
    fn exact_out_never_exceeds_requested() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let requested = 1_000_000_000_000u128;

        let result = pool
            .simulate_swap(make_params(
                true,
                SignedAmount::positive(U256::from(requested)),
                sqrt_at(-120),
            ))
            .expect("exact-out swap should succeed");

        assert!(
            delta_amount_out(&result.delta, true) <= requested,
            "pool must never send more than was requested"
        );
    }

    #[test]
    fn crossings_populated_on_tick_crossing() {
        let liq = 500_000_000_000_000_000u128;
        let pool = one_range_pool(liq);

        let result = pool
            .simulate_swap_full(make_params(
                true,
                SignedAmount::negative(ether(100)),
                sqrt_at(-240),
            ))
            .unwrap();

        // The swap crossed tick -120, so crossing metadata must be present.
        assert_eq!(result.crossings.len(), 1);
        assert_eq!(result.crossings[0].tick, tick(-120));

        // Cumulative input is bounded by total input.
        let total_input = U256::from(delta_amount_in(&result.delta, true));
        assert!(result.crossings[0].cumulative_input > U256::ZERO);
        assert!(result.crossings[0].cumulative_input <= total_input);

        // Liquidity is drained below the active range.
        assert_eq!(result.crossings[0].liquidity_after, Liquidity::ZERO);
    }

    #[test]
    fn crossings_empty_when_no_tick_crossed() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);

        // A small swap should stay inside the current range.
        let result = pool
            .simulate_swap_full(make_params(
                true,
                SignedAmount::negative(U256::from(1_000u128)),
                sqrt_at(-1),
            ))
            .unwrap();

        assert!(result.crossings.is_empty());
    }

    #[test]
    fn crossings_empty_for_exact_output() {
        let liq = 500_000_000_000_000_000u128;
        let pool = one_range_pool(liq);

        let result = pool
            .simulate_swap_full(make_params(
                true,
                SignedAmount::positive(U256::from(1_000_000u128)),
                sqrt_at(-240),
            ))
            .unwrap();

        // Exact-output crossings do not accumulate input metadata.
        for c in &result.crossings {
            assert_eq!(c.cumulative_input, U256::ZERO);
        }
    }

    fn one_range_pool(liq: u128) -> Pool {
        make_pool(
            sqrt_at(0),
            0,
            liq,
            3_000,
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
        let pool = one_range_pool(liq);

        let result = pool
            .simulate_swap(make_params(
                true,
                SignedAmount::negative(ether(100)),
                sqrt_at(-240),
            ))
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
        let pool = one_range_pool(liq);

        let result = pool
            .simulate_swap(make_params(
                false,
                SignedAmount::negative(ether(100)),
                sqrt_at(240),
            ))
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
        let pool = make_pool(sqrt_price_1_1(), 0, 0, 3_000, tick_spacing_60(), []);
        let liq = 1_000_000_000_000u128;

        let result = pool
            .modify_liquidity(ModifyLiquidityParams::new(
                tick(-120),
                tick(120),
                liq as i128,
            ))
            .expect("add should succeed");

        assert_eq!(result.liquidity, Liquidity::new(liq));
        // Liquidity delta is caller / PoolManager perspective: caller owes tokens when LP adds.
        assert!(result.delta.amount0 < 0, "caller pays token0");
        assert!(result.delta.amount1 < 0, "caller pays token1");
        assert!(result.flipped_lower && result.flipped_upper);

        let ticks = pool.ticks.read();
        assert_eq!(ticks.get(tick(-120)).unwrap().liquidity_net, liq as i128);
        assert_eq!(ticks.get(tick(120)).unwrap().liquidity_net, -(liq as i128));
    }

    #[test]
    fn add_liquidity_below_range_only_token0() {
        let pool = make_pool(sqrt_at(-240), -240, 0, 3_000, tick_spacing_60(), []);

        let result = pool
            .modify_liquidity(ModifyLiquidityParams::new(
                tick(-120),
                tick(120),
                1_000_000_000_000i128,
            ))
            .expect("add should succeed");

        assert_eq!(result.liquidity, Liquidity::ZERO, "out-of-range position");

        // Price below range: adding liquidity requires only token0 from the caller.
        assert!(result.delta.amount0 < 0, "caller pays token0");
        assert_eq!(result.delta.amount1, 0);
    }

    #[test]
    fn remove_liquidity_clears_ticks() {
        let pool = make_pool(sqrt_price_1_1(), 0, 0, 3_000, tick_spacing_60(), []);
        let liq = 1_000_000_000_000i128;

        pool.modify_liquidity(ModifyLiquidityParams::new(tick(-120), tick(120), liq))
            .unwrap();

        let result = pool
            .modify_liquidity(ModifyLiquidityParams::new(tick(-120), tick(120), -liq))
            .expect("remove should succeed");

        assert_eq!(result.liquidity, Liquidity::ZERO);
        // Removing liquidity means the caller receives tokens back from the pool.
        assert!(result.delta.amount0 > 0, "caller receives token0");
        assert!(result.delta.amount1 > 0, "caller receives token1");
        assert!(result.flipped_lower && result.flipped_upper);

        let ticks = pool.ticks.read();
        assert!(ticks.get(tick(-120)).is_none());
        assert!(ticks.get(tick(120)).is_none());
    }

    #[test]
    fn rejects_out_of_range_state_tick() {
        assert!(TickIndex::new(MIN_TICK - 1).is_none());
    }

    #[test]
    #[should_panic(expected = "tick index out of range")]
    fn rejects_out_of_range_tick_entry() {
        PoolTicks::from_snapshot(tick_spacing_60(), [tick_info(MIN_TICK - 1, 0, 1)].into())
            .unwrap_err();
    }

    #[test]
    fn exact_input_allows_100_percent_fee() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        pool.fee = 1_000_000;

        let result = pool
            .simulate_swap(make_params(
                true,
                SignedAmount::negative(ether(1)),
                sqrt_at(-60),
            ))
            .expect("V4 allows 100% fee for exact-input");

        // Entire input consumed as fee; no output.
        assert_eq!(delta_amount_in(&result.delta, true), ether(1).to::<u128>());
        assert_eq!(delta_amount_out(&result.delta, true), 0);
    }

    #[test]
    fn exact_output_rejects_100_percent_fee() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        pool.fee = 1_000_000;

        assert_eq!(
            pool.simulate_swap(make_params(
                true,
                SignedAmount::positive(U256::from(1_000_000u128)),
                sqrt_at(-60),
            ))
            .unwrap_err(),
            SwapSimError::FeeTooLarge
        );
    }

    #[test]
    fn rejects_limit_wrong_side_for_zero_for_one() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);

        assert_eq!(
            pool.simulate_swap(make_params(
                true,
                SignedAmount::negative(ether(1)),
                sqrt_at(1),
            ))
            .unwrap_err(),
            SwapSimError::PriceLimitAlreadyExceeded
        );
    }

    #[test]
    fn rejects_limit_wrong_side_for_one_for_zero() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);

        assert_eq!(
            pool.simulate_swap(make_params(
                false,
                SignedAmount::negative(ether(1)),
                sqrt_at(-1),
            ))
            .unwrap_err(),
            SwapSimError::PriceLimitAlreadyExceeded
        );
    }

    #[test]
    fn rejects_limit_below_absolute_minimum() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);

        assert_eq!(
            pool.simulate_swap(make_params(
                true,
                SignedAmount::negative(ether(1)),
                invalid_sqrt_from_u160(test_min_sqrt_price() - U160::ONE),
            ))
            .unwrap_err(),
            SwapSimError::PriceLimitOutOfBounds
        );
    }

    #[test]
    fn rejects_invalid_pool_sqrt_price() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        pool.state.write().sqrt_price_x96 =
            invalid_sqrt_from_u160(test_min_sqrt_price() - U160::ONE);

        assert_eq!(
            pool.simulate_swap(make_params(
                true,
                SignedAmount::negative(ether(1)),
                sqrt_from_u160(test_min_sqrt_price()),
            ))
            .unwrap_err(),
            SwapSimError::InvalidPoolSqrtPrice
        );
    }

    fn tick_pool_for_search() -> PoolTicks {
        PoolTicks::from_snapshot(
            tick_spacing_60(),
            [
                tick_info(-240, 0, 1),
                tick_info(0, 0, 1),
                tick_info(240, 0, 1),
            ]
            .into(),
        )
        .expect("valid ticks")
    }

    #[test]
    fn next_tick_down_returns_tick_at_current() {
        let ticks = tick_pool_for_search();
        let guard = ticks.read();
        let next = guard.next_initialized_tick(tick(0), true);
        assert_eq!(next.tick_next, tick(0));
        assert!(next.initialized);
    }

    #[test]
    fn next_tick_up_skips_current() {
        let ticks = tick_pool_for_search();
        let guard = ticks.read();
        let next = guard.next_initialized_tick(tick(0), false);
        assert_eq!(next.tick_next, tick(240));
        assert!(next.initialized);
    }

    #[test]
    fn next_tick_returns_sentinel_when_empty() {
        let ticks =
            PoolTicks::from_snapshot(tick_spacing_60(), [tick_info(120, 0, 1)].into()).unwrap();
        let guard = ticks.read();

        let down = guard.next_initialized_tick(tick(50), true);
        assert_eq!(down.tick_next, TickIndex::MIN);
        assert!(!down.initialized);

        let up = guard.next_initialized_tick(tick(120), false);
        assert_eq!(up.tick_next, TickIndex::MAX);
        assert!(!up.initialized);
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
            let pool = make_pool(sqrt_at(0), 0, liq, fee, tick_spacing_60(), ticks);

            let limit_tick = if zero_for_one { -limit_tick_offset } else { limit_tick_offset };
            let limit = sqrt_at(limit_tick);
            let amount = if exact_in {
                SignedAmount::negative(U256::from(amount_raw))
            } else {
                SignedAmount::positive(U256::from(amount_raw))
            };

            let result = pool
                .simulate_swap(make_params(zero_for_one, amount, limit))
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
            let pool = make_pool(sqrt_at(0), 0, liq, 3_000, tick_spacing_60(), ticks);

            let result = pool
                .simulate_swap(make_params(
                    zero_for_one,
                    SignedAmount::negative(U256::from(amount_raw)),
                    extreme_price_limit(zero_for_one),
                ))
                .expect("swap should succeed");

            // amount_in must equal the unsigned abs of the negative input raw delta
            // (caller perspective: negative = caller owes/pays).
            if zero_for_one {
                prop_assert_eq!(
                    delta_amount_in(&result.delta, true),
                    result.delta.amount0.unsigned_abs(),
                    "amount_in(true) must equal |amount0|"
                );
            } else {
                prop_assert_eq!(
                    delta_amount_in(&result.delta, false),
                    result.delta.amount1.unsigned_abs(),
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
            let pool = make_pool(sqrt_at(0), 0, liq, 3_000, tick_spacing_60(), ticks);

            let result = pool
                .simulate_swap(make_params(
                    zero_for_one,
                    SignedAmount::positive(U256::from(amount_raw)),
                    extreme_price_limit(zero_for_one),
                ))
                .expect("swap should succeed");

            prop_assert!(
                delta_amount_out(&result.delta, zero_for_one) <= amount_raw,
                "output exceeded requested amount"
            );
        }
    }
}
