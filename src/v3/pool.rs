//! Uniswap V4-faithful full-swap simulator with tick-crossing loop.
//!
//! This module is a direct Rust translation of Uniswap V4's
//! [`Pool.sol`](https://github.com/Uniswap/v4-core/blob/main/src/libraries/Pool.sol).
//! The loop structure, amount-accounting variables, tick-crossing logic, and
//! final `BalanceDelta` assembly mirror the Solidity source exactly.
//!
//! V4 pools use identical swap math to V3; the only structural difference is
//! that `tick_spacing` comes from `PoolKey` rather than being inferred from
//! the fee tier.
//!
//! # BalanceDelta sign convention
//!
//! `Pool.swap` in V4 returns a `BalanceDelta` from the **caller / PoolManager
//! accounting perspective**, matching Solidity's final `toBalanceDelta` assembly.
//!
//! | Field value | Caller-perspective meaning                         |
//! |-------------|----------------------------------------------------|
//! | `< 0`       | Caller owes/sends this token to the PoolManager.   |
//! | `> 0`       | Caller receives/is owed this token from the pool.  |
//!
//! Concretely, for a `zero_for_one` exact-input swap:
//! - `delta.amount0 < 0` — caller paid token0 into the pool.
//! - `delta.amount1 > 0` — caller receives token1 from the pool.
//!
//! Use the local `delta_amount_in` / `delta_amount_out` helpers below for
//! direction-aware unsigned magnitudes.
//!
//! # Internal `amountCalculated` convention
//!
//! Mirrors V4 Solidity exactly:
//! - **Exact-input**: `amountCalculated += step.amountOut` → accumulates positive output.
//! - **Exact-output**: `amountCalculated -= step.amountIn + step.feeAmount` → accumulates negative input.
//!
//! # Changelog
//!
//! ### This revision — V4 fidelity rewrite
//!
//! * **Sign convention aligned with V4** — `BalanceDelta` now uses the caller /
//!   PoolManager accounting perspective returned by `Pool.swap` exactly:
//!   negative input token, positive output token.
//!
//! * **Direction-aware amount helpers corrected** — output checks `raw > 0`
//!   and input checks `raw < 0`, consistent with v4 flash-accounting deltas.
//!
//! * **Comment bug fixed** — the exact-input branch in the swap loop was
//!   labelled `// if exactOutput`; corrected to `// exact-input branch`.
//!
//! * **`liquidityNet` negation made explicit** — V4's `Pool.swap` explicitly
//!   negates `liquidityNet` when `zeroForOne` before calling
//!   `LiquidityMath.addDelta`. The previous revision hid this inside
//!   `cross_tick`, making the V4 symmetry invisible at the call site.
//!   The negation is now applied in the swap loop body with a clear comment.
//!
//! * **Protocol fee deduction ordering corrected** — V4 updates swap amount
//!   accounting with the full `step.amountIn + step.feeAmount` first, then
//!   deducts the protocol share only from the LP fee-growth amount.
//!
//! * **Fee validation moved before price-limit validation** — `validate_fee`
//!   and `validate_swap_fee_for_exactness` now run before the price-limit
//!   check, matching V4's revert ordering exactly.
//!
//! * **`MAX_SWAP_ITERATIONS` corrected** — constant and doc comment were
//!   inconsistent (10 000 vs 1 024). The constant is now 10 000 with a note
//!   that V4 has no library-level cap (EVM gas is the bound), and 10 000 is
//!   the simulator's defensive ceiling for malformed local state.
//!
//! * **Tick boundary clamping kept V4-faithful** — V4 clamps `tickNext` to
//!   `[MIN_TICK, MAX_TICK]` before calling `getSqrtPriceAtTick`, because the
//!   bitmap itself is not aware of the min/max tick bounds.
//!
//! ### Preserved from previous revision
//!
//! * `signed_amount_to_i128` correctly handles `i128::MIN` (magnitude 2^127).
//! * Direction-aware amount helpers return 0 on sign violation rather than
//!   a spurious non-zero value.
//! * `state.tick` and `TickEntry.tick_idx` are range-validated on entry.
//! * Protocol-fee `mul_div` used instead of `saturating_mul` to prevent
//!   silent precision loss.
//! * `protocol_fee_amount` field present on `FullSwapResult` under
//!   `#[cfg(feature = "protocol-fee")]`.
//! * `validate_fee` called before any other work.
//! * `tick_spacing` validated on entry.

use std::sync::Arc;

use parking_lot::RwLock;
use ruint::aliases::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::v3::cache::PoolCache;
use crate::v3::price::PriceCache;
use crate::v3::sqrt_price_math::{
    get_amount0_delta, get_amount1_delta, get_next_sqrt_price_from_input,
    get_next_sqrt_price_from_output,
};
use crate::v3::types::BalanceDelta;
use crate::v3::{
    amount::SignedAmount,
    error::SwapSimError,
    tick_math::{MAX_SQRT_PRICE, MAX_TICK_SPACING, MIN_SQRT_PRICE},
    ticks::{NextInitializedTick, PoolTicks, PoolTicksReadGuard},
};

use super::{
    full_math::MathError,
    swap_math::{SwapMathError, get_sqrt_price_target},
    tick_math::{MAX_TICK, MIN_TICK, get_sqrt_price_at_tick, get_tick_at_sqrt_price},
};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Fee denominator used by Uniswap-style fee pips.
///
/// A fee of `500` means `500 / 1_000_000`, or `0.05%`.
const FEE_DENOMINATOR: f64 = 1_000_000.0;

const Q96_F64: f64 = 7.922_816_251_426_434e28;

/// Minimum valid tick spacing (1 = finest granularity).
pub const MIN_TICK_SPACING: i32 = 1;

/// Maximum number of swap-loop iterations per simulation.
///
/// Uniswap V4 itself has no library-level iteration cap; the EVM gas limit is
/// the practical bound. This simulator's defensive ceiling prevents malformed
/// local state from spinning indefinitely. 10 000 is far beyond any realistic
/// single swap on mainnet tick densities.
pub const MAX_SWAP_ITERATIONS: usize = 10_000;

/// Maximum swap fee in pips (100 % = 1 000 000).
const MAX_SWAP_FEE: u32 = 1_000_000;

/// Trait for tick-crossing guards used in swap simulations.
///
/// The default `next_initialized_tick_within_one_word` implementation preserves
/// the current ordered-map behavior. A future bitmap-backed `PoolTicks` guard can
/// override only that method and immediately speed up the swap loop without
/// changing the executor.
pub trait TickCrossing {
    fn next_initialized_tick(&self, current_tick: i32, zero_for_one: bool) -> NextInitializedTick;
    fn cross_tick(&self, tick_next: i32) -> Result<i128, SwapSimError>;

    #[inline(always)]
    fn next_initialized_tick_within_one_word(
        &self,
        current_tick: i32,
        tick_spacing: i32,
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
    fn next_initialized_tick(&self, current_tick: i32, zero_for_one: bool) -> NextInitializedTick {
        PoolTicksReadGuard::next_initialized_tick(self, current_tick, zero_for_one)
    }

    #[inline]
    fn cross_tick(&self, tick_next: i32) -> Result<i128, SwapSimError> {
        PoolTicksReadGuard::cross_tick(self, tick_next)
    }
}

#[cfg(feature = "protocol-fee")]
mod protocol_fee_consts {
    /// `ProtocolFeeLibrary.MAX_PROTOCOL_FEE` = 1 000 (0.1 %).
    pub const MAX_PROTOCOL_FEE: u32 = 1_000;
    /// `ProtocolFeeLibrary.PIPS_DENOMINATOR` = 1 000 000.
    pub const PIPS_DENOMINATOR: u32 = 1_000_000;
}

// ─── Pool state ───────────────────────────────────────────────────────────────

/// Complete V3/V4 pool state required for a full tick-crossing simulation.
///
/// # Invariants (caller-enforced)
///
/// * `tick == floor(log_√1.0001(sqrt_price_x96))`. The simulator does not
///   re-derive tick from price; a mismatch produces wrong output.
/// * `fee <= 1_000_000` — enforced on entry by [`Pool::swap`].
/// * `tick_spacing ∈ [MIN_TICK_SPACING, MAX_TICK_SPACING]` — enforced on entry.
/// * All `tick_idx` values in `ticks` must be within `[MIN_TICK, MAX_TICK]` and
///   be multiples of `tick_spacing` — enforced at `PoolTicks` construction.
#[derive(Debug, Clone)]
pub struct Pool {
    pub state: Arc<RwLock<PoolState>>,

    /// Fee tier in pips (e.g. `3_000` = 0.30 %). Must be `<= 1_000_000`.
    pub fee: u32,

    /// Tick spacing. V3: derived from fee. V4: explicit from `PoolKey.tickSpacing`.
    /// Must lie in `[MIN_TICK_SPACING, MAX_TICK_SPACING]`.
    pub tick_spacing: i32,

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
        let state_guard = self.state.read();

        #[derive(Serialize)]
        struct PoolSnapshot<'a> {
            state: &'a PoolState,
            fee: u32,
            tick_spacing: i32,
            ticks: &'a PoolTicks,
        }

        PoolSnapshot {
            state: &*state_guard,
            fee: self.fee,
            tick_spacing: self.tick_spacing,
            ticks: &self.ticks,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Pool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct PoolSnapshot {
            state: PoolState,
            fee: u32,
            tick_spacing: i32,
            ticks: PoolTicks,
        }

        let snapshot = PoolSnapshot::deserialize(deserializer)?;
        let sqrt_price_x96 = snapshot.state.sqrt_price_x96;

        let cache = Arc::new(PoolCache::default());

        Ok(Self {
            state: Arc::new(RwLock::new(snapshot.state)),
            fee: snapshot.fee,
            tick_spacing: snapshot.tick_spacing,
            ticks: snapshot.ticks,
            cache,
            price_cache: Arc::new(PriceCache::new(
                sqrt_price_x96_to_price(sqrt_price_x96),
                snapshot.fee,
            )),
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PoolState {
    /// Current pool sqrt price in Q64.96 fixed-point.
    pub sqrt_price_x96: U256,

    /// Current tick — must equal `floor(log_√1.0001(sqrt_price_x96))`.
    pub tick: i32,

    /// Active liquidity for the current tick range (`uint128` matching V3/V4).
    pub liquidity: u128,
}

// ─── Swap parameters ──────────────────────────────────────────────────────────

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
    pub sqrt_price_limit_x96: U256,

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
    pub tick_lower: i32,
    /// Upper tick of the position range.
    pub tick_upper: i32,
    /// Signed liquidity delta. Positive adds liquidity, negative removes it.
    pub liquidity_delta: i128,
}

impl ModifyLiquidityParams {
    pub fn new(tick_lower: i32, tick_upper: i32, liquidity_delta: i128) -> Self {
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
    pub liquidity: u128,

    /// Whether the lower tick flipped initialized/uninitialized state.
    pub flipped_lower: bool,

    /// Whether the upper tick flipped initialized/uninitialized state.
    pub flipped_upper: bool,
}

// ─── Result types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickCrossInfo {
    pub cumulative_input: U256,
    pub tick: i32,
    pub liquidity_after: u128,
}

/// Output of a successful simulated swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullSwapResult {
    /// Pool sqrt price after the swap, in Q64.96.
    pub sqrt_price_x96: U256,

    /// Pool tick after the swap.
    pub tick: i32,

    /// Active liquidity after the swap. May differ from the input value
    /// if tick boundaries were crossed.
    pub liquidity: u128,

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
    pub sqrt_price_x96: U256,

    /// Pool tick after the simulated swap.
    pub tick: i32,

    /// Active liquidity after the simulated swap.
    pub liquidity: u128,

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

// ─── Pool APIs ────────────────────────────────────────────────────────────────

impl Pool {
    pub fn new(
        sqrt_price_x96: U256,
        tick: i32,
        liquidity: u128,
        fee: u32,
        tick_spacing: i32,
        ticks: PoolTicks,
    ) -> Self {
        let cache = Arc::new(PoolCache::default());

        Pool {
            state: Arc::new(RwLock::new(PoolState {
                sqrt_price_x96,
                tick,
                liquidity,
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

    /// Apply a liquidity change and commit it to pool state.
    ///
    /// Implements the pool-level portion of Uniswap V4's `modifyLiquidity`:
    ///
    /// 1. Validate `tick_lower < tick_upper` and tick spacing alignment.
    /// 2. Update lower/upper ticks with the signed `liquidity_net` convention.
    /// 3. Enforce `tick_spacing_to_max_liquidity_per_tick` when adding.
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
        validate_tick_spacing(self.tick_spacing)?;
        if self.ticks.tick_spacing() != self.tick_spacing {
            return Err(SwapSimError::InvalidTickSpacing);
        }
        check_ticks(params.tick_lower, params.tick_upper, self.tick_spacing)?;

        // Lock order: ticks first, then pool state — must match Pool::swap to
        // prevent deadlock if both are called concurrently.
        let mut ticks = self.ticks.write();
        let mut state = self.state.write();
        validate_tick_range(state.tick)?;

        let mut flipped_lower = false;
        let mut flipped_upper = false;

        if params.liquidity_delta != 0 {
            let max_liquidity_per_tick = if params.liquidity_delta > 0 {
                Some(tick_spacing_to_max_liquidity_per_tick(self.tick_spacing))
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
        }

        let mut delta = BalanceDelta::default();

        if params.liquidity_delta != 0 {
            let sqrt_price_lower_x96 = self.cache.get_sqrt_price_at_tick(params.tick_lower)?;
            let sqrt_price_upper_x96 = self.cache.get_sqrt_price_at_tick(params.tick_upper)?;

            if state.tick < params.tick_lower {
                // Price is below the range — only token0 is involved.
                delta.amount0 = get_amount0_delta_signed(
                    sqrt_price_lower_x96,
                    sqrt_price_upper_x96,
                    params.liquidity_delta,
                )?;
            } else if state.tick < params.tick_upper {
                // Price is inside the range — both tokens and active liquidity change.
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
                state.liquidity = add_liquidity_delta(state.liquidity, params.liquidity_delta)?;
            } else {
                // Price is above the range — only token1 is involved.
                delta.amount1 = get_amount1_delta_signed(
                    sqrt_price_lower_x96,
                    sqrt_price_upper_x96,
                    params.liquidity_delta,
                )?;
            }
        }

        self.cache.clear_cross_amounts();

        Ok(ModifyLiquidityResult {
            delta,
            liquidity: state.liquidity,
            flipped_lower,
            flipped_upper,
        })
    }

    /// Execute a swap and commit the resulting state to this pool.
    ///
    /// This mutating API preserves the full result shape, including crossings,
    /// because callers that mutate pool state often need audit/debug metadata.
    /// For the fastest read-only quote path, use [`Pool::simulate_swap`].
    pub fn swap(&self, params: SwapParams) -> Result<FullSwapResult, SwapSimError> {
        let prepared = self.prepare_swap(&params)?;
        let mut state = self.state.write();

        let result = execute_swap_full_from_state(
            &state,
            &prepared.ticks,
            prepared.cache,
            prepared.effective_fee,
            self.tick_spacing,
            params,
        )?;

        // Commit only after the full simulation succeeds.
        state.sqrt_price_x96 = result.sqrt_price_x96;
        state.tick = result.tick;
        if state.liquidity != result.liquidity {
            state.liquidity = result.liquidity;
        }

        self.cache.clear_cross_amounts();

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
    ) -> Result<(u128, U256, i32, u128), SwapSimError> {
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
        validate_tick_spacing(self.tick_spacing)?;

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

// ─── Swap loop state ──────────────────────────────────────────────────────────

/// Mutable loop state shared by the light and full executors.
#[derive(Debug, Clone, Copy)]
struct SwapLoopState {
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
}

// ─── Core swap executors ─────────────────────────────────────────────────────

#[inline]
fn execute_swap_light_from_state<G: TickCrossing>(
    state: &PoolState,
    ticks: &G,
    cache: &PoolCache,
    effective_swap_fee: u32,
    tick_spacing: i32,
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
    tick_spacing: i32,
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
    tick_spacing: i32,
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
    sqrt_ratio_next_x96: U256,
    amount_in: U256,
    amount_out: U256,
    fee_amount: U256,
}

/// Exact U256 × u32 / u32 long division.
///
/// Fee arithmetic only needs a small multiplier and denominator. Using this
/// helper avoids the generic FullMath 512-bit modulo/inverse path in the
/// default simulation loop.
#[inline(always)]
fn mul_div_u32_div_rem_math(a: U256, mul: u32, div: u32) -> Result<(U256, u32), MathError> {
    if div == 0 {
        return Err(MathError::ZeroDenominator);
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
        return Err(MathError::Overflow);
    }

    Ok((U256::from_limbs([q[0], q[1], q[2], q[3]]), rem as u32))
}

#[inline(always)]
fn mul_div_u32_floor_math(a: U256, mul: u32, div: u32) -> Result<U256, MathError> {
    let (q, _) = mul_div_u32_div_rem_math(a, mul, div)?;
    Ok(q)
}

#[inline(always)]
fn mul_div_u32_ceil_math(a: U256, mul: u32, div: u32) -> Result<U256, MathError> {
    let (q, r) = mul_div_u32_div_rem_math(a, mul, div)?;
    if r == 0 {
        Ok(q)
    } else {
        q.checked_add(U256::ONE).ok_or(MathError::Overflow)
    }
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

    Ok(mul_div_u32_floor_math(
        amount_remaining,
        MAX_SWAP_FEE - fee_pips,
        MAX_SWAP_FEE,
    )?)
}

#[inline(always)]
fn fee_on_exact_input_fast(amount_in: U256, fee_pips: u32) -> Result<U256, SwapMathError> {
    if amount_in.is_zero() || fee_pips == 0 {
        return Ok(U256::ZERO);
    }
    if fee_pips == MAX_SWAP_FEE {
        return Ok(amount_in);
    }

    Ok(mul_div_u32_ceil_math(
        amount_in,
        fee_pips,
        MAX_SWAP_FEE - fee_pips,
    )?)
}

/// Exact-input swap-step calculation for the lightweight pool loop.
///
/// The public `compute_swap_step` must parse a signed amount and dispatch to
/// exact-in/exact-out every call. The simulator loop already knows exactness,
/// direction, fee, and liquidity, so this function skips that generic layer.
#[inline]
fn compute_swap_step_exact_in_fast(
    sqrt_ratio_current_x96: U256,
    sqrt_ratio_target_x96: U256,
    liquidity: u128,
    amount_remaining: U256,
    fee_pips: u32,
    zero_for_one: bool,
) -> Result<LightSwapStep, SwapMathError> {
    let liquidity_u = U256::from(liquidity);
    let amount_remaining_less_fee = amount_less_fee_exact_in_fast(amount_remaining, fee_pips)?;

    let max_amount_in = if zero_for_one {
        get_amount0_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity_u,
            true,
        )?
    } else {
        get_amount1_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity_u,
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
            liquidity_u,
            amount_remaining_less_fee,
            zero_for_one,
        )?;
        (next, amount_remaining_less_fee)
    };

    let amount_out = if zero_for_one {
        get_amount1_delta(
            sqrt_ratio_next_x96,
            sqrt_ratio_current_x96,
            liquidity_u,
            false,
        )?
    } else {
        get_amount0_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_next_x96,
            liquidity_u,
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

fn u256_to_f64(v: U256) -> f64 {
    let limbs = v.into_limbs();
    if limbs[2] == 0 && limbs[3] == 0 {
        let lo = (limbs[0] as u128) | ((limbs[1] as u128) << 64);
        return lo as f64;
    }
    (limbs[0] as f64)
        + (limbs[1] as f64) * 1.844_674_407_370_955_2e19
        + (limbs[2] as f64) * 3.402_823_669_209_384_6e38
        + (limbs[3] as f64) * 6.277_101_735_386_680e57
}

#[inline]
pub fn sqrt_price_x96_to_price(sqrt_price_x96: U256) -> f64 {
    let sqrt_price = u256_to_f64(sqrt_price_x96) / Q96_F64;

    sqrt_price * sqrt_price
}

/// Exact-output swap-step calculation for the lightweight pool loop.
#[inline]
fn compute_swap_step_exact_out_fast(
    sqrt_ratio_current_x96: U256,
    sqrt_ratio_target_x96: U256,
    liquidity: u128,
    amount_remaining: U256,
    fee_pips: u32,
    zero_for_one: bool,
) -> Result<LightSwapStep, SwapMathError> {
    debug_assert!(fee_pips < MAX_SWAP_FEE);

    let liquidity_u = U256::from(liquidity);

    let max_amount_out = if zero_for_one {
        get_amount1_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity_u,
            false,
        )?
    } else {
        get_amount0_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity_u,
            false,
        )?
    };

    let (sqrt_ratio_next_x96, amount_out) = if amount_remaining >= max_amount_out {
        (sqrt_ratio_target_x96, max_amount_out)
    } else {
        let next = get_next_sqrt_price_from_output(
            sqrt_ratio_current_x96,
            liquidity_u,
            amount_remaining,
            zero_for_one,
        )?;
        (next, amount_remaining)
    };

    let amount_in = if zero_for_one {
        get_amount0_delta(
            sqrt_ratio_next_x96,
            sqrt_ratio_current_x96,
            liquidity_u,
            true,
        )?
    } else {
        get_amount1_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_next_x96,
            liquidity_u,
            true,
        )?
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
fn mul_div_u32_floor(a: U256, mul: u32, div: u32) -> Result<U256, SwapSimError> {
    if div == 0 {
        return Err(SwapSimError::AmountOverflow);
    }
    if a.is_zero() || mul == 0 {
        return Ok(U256::ZERO);
    }
    if mul == div {
        return Ok(a);
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

    if q[4] != 0 {
        return Err(SwapSimError::AmountOverflow);
    }

    Ok(U256::from_limbs([q[0], q[1], q[2], q[3]]))
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
    tick_next: i32,
    tick_initialized: bool,
    sqrt_boundary: U256,
    sqrt_target: U256,
    #[cfg(feature = "protocol-fee")] total_protocol_fee: &mut U256,
) -> Result<bool, SwapSimError> {
    // If price limit is before the tick boundary, the cached boundary-cross amount is invalid.
    if sqrt_target != sqrt_boundary {
        return Ok(false);
    }

    let Some(entry) = cache.get_or_compute_boundary_cross(
        tick_next,
        params.zero_for_one,
        result.sqrt_price_x96,
        sqrt_boundary,
        result.liquidity,
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

    #[cfg(feature = "protocol-fee")]
    {
        let mut step_fee_amount = entry.fee_amount_to_cross;
        deduct_protocol_fee_from_step(
            entry.amount_in_to_cross,
            &mut step_fee_amount,
            effective_swap_fee,
            params.protocol_fee,
            total_protocol_fee,
        )?;
    }

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
            });
        }
    }

    result.tick = if params.zero_for_one {
        tick_next.wrapping_sub(1)
    } else {
        tick_next
    };

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
    tick_next: i32,
    tick_initialized: bool,
    sqrt_boundary: U256,
    sqrt_target: U256,
    #[cfg(feature = "protocol-fee")] total_protocol_fee: &mut U256,
) -> Result<bool, SwapSimError> {
    // If price limit is before the tick boundary, the cached boundary-cross amount is invalid.
    if sqrt_target != sqrt_boundary {
        return Ok(false);
    }

    let Some(entry) = cache.get_or_compute_boundary_cross(
        tick_next,
        params.zero_for_one,
        result.sqrt_price_x96,
        sqrt_boundary,
        result.liquidity,
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

    #[cfg(feature = "protocol-fee")]
    {
        let mut step_fee_amount = entry.fee_amount_to_cross;
        deduct_protocol_fee_from_step(
            entry.amount_in_to_cross,
            &mut step_fee_amount,
            effective_swap_fee,
            params.protocol_fee,
            total_protocol_fee,
        )?;
    }

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
            });
        }
    }

    result.tick = if params.zero_for_one {
        tick_next.wrapping_sub(1)
    } else {
        tick_next
    };

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
    tick_spacing: i32,
    params: SwapParams,
) -> Result<SwapSimulationResult, SwapSimError> {
    let mut result = SwapLoopState {
        sqrt_price_x96: state.sqrt_price_x96,
        tick: state.tick,
        liquidity: state.liquidity,
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
        let tick_next = if next_tick.tick_next <= MIN_TICK {
            MIN_TICK
        } else if next_tick.tick_next >= MAX_TICK {
            MAX_TICK
        } else {
            next_tick.tick_next
        };

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

            result.tick = if params.zero_for_one {
                tick_next.wrapping_sub(1)
            } else {
                tick_next
            };
        } else if result.sqrt_price_x96 != sqrt_start {
            // A non-boundary price movement is terminal for this step: either
            // the specified amount was exhausted or the price limit was hit.
            result.tick = get_tick_at_sqrt_price(result.sqrt_price_x96)?;
            break;
        }
    }

    let input_received = checked_sub_u256(amount_specified, amount_remaining)?;
    let delta = make_delta_from_exact_in(params.zero_for_one, input_received, amount_out_accum)?;

    Ok(SwapSimulationResult {
        sqrt_price_x96: result.sqrt_price_x96,
        tick: result.tick,
        liquidity: result.liquidity,
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
    tick_spacing: i32,
    params: SwapParams,
) -> Result<SwapSimulationResult, SwapSimError> {
    let mut result = SwapLoopState {
        sqrt_price_x96: state.sqrt_price_x96,
        tick: state.tick,
        liquidity: state.liquidity,
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
        let tick_next = if next_tick.tick_next <= MIN_TICK {
            MIN_TICK
        } else if next_tick.tick_next >= MAX_TICK {
            MAX_TICK
        } else {
            next_tick.tick_next
        };

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

            result.tick = if params.zero_for_one {
                tick_next.wrapping_sub(1)
            } else {
                tick_next
            };
        } else if result.sqrt_price_x96 != sqrt_start {
            // A non-boundary price movement is terminal for this step: either
            // the specified amount was exhausted or the price limit was hit.
            result.tick = get_tick_at_sqrt_price(result.sqrt_price_x96)?;
            break;
        }
    }

    let output_sent = checked_sub_u256(amount_specified, amount_remaining)?;
    let delta = make_delta_from_exact_out(params.zero_for_one, amount_in_accum, output_sent)?;

    Ok(SwapSimulationResult {
        sqrt_price_x96: result.sqrt_price_x96,
        tick: result.tick,
        liquidity: result.liquidity,
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
    tick_spacing: i32,
    params: SwapParams,
) -> Result<(SwapSimulationResult, Option<Vec<TickCrossInfo>>), SwapSimError> {
    let mut result = SwapLoopState {
        sqrt_price_x96: state.sqrt_price_x96,
        tick: state.tick,
        liquidity: state.liquidity,
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
        let tick_next = if next_tick.tick_next <= MIN_TICK {
            MIN_TICK
        } else if next_tick.tick_next >= MAX_TICK {
            MAX_TICK
        } else {
            next_tick.tick_next
        };

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

                if RECORD_CROSSINGS {
                    if let Some(crossings) = crossings.as_mut() {
                        crossings.push(TickCrossInfo {
                            cumulative_input: checked_sub_u256(amount_specified, amount_remaining)?,
                            tick: tick_next,
                            liquidity_after,
                        });
                    }
                }
            }

            result.tick = if params.zero_for_one {
                tick_next.wrapping_sub(1)
            } else {
                tick_next
            };
        } else if result.sqrt_price_x96 != sqrt_start {
            if amount_remaining.is_zero() || result.sqrt_price_x96 == params.sqrt_price_limit_x96 {
                result.tick = get_tick_at_sqrt_price(result.sqrt_price_x96)?;
                break;
            }

            result.tick = get_tick_at_sqrt_price(result.sqrt_price_x96)?;
        }
    }

    let input_received = checked_sub_u256(amount_specified, amount_remaining)?;
    let delta = make_delta_from_exact_in(params.zero_for_one, input_received, amount_out_accum)?;

    let light = SwapSimulationResult {
        sqrt_price_x96: result.sqrt_price_x96,
        tick: result.tick,
        liquidity: result.liquidity,
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
    tick_spacing: i32,
    params: SwapParams,
) -> Result<(SwapSimulationResult, Option<Vec<TickCrossInfo>>), SwapSimError> {
    let mut result = SwapLoopState {
        sqrt_price_x96: state.sqrt_price_x96,
        tick: state.tick,
        liquidity: state.liquidity,
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
        let tick_next = if next_tick.tick_next <= MIN_TICK {
            MIN_TICK
        } else if next_tick.tick_next >= MAX_TICK {
            MAX_TICK
        } else {
            next_tick.tick_next
        };

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

                if RECORD_CROSSINGS {
                    if let Some(crossings) = crossings.as_mut() {
                        crossings.push(TickCrossInfo {
                            cumulative_input: U256::ZERO,
                            tick: tick_next,
                            liquidity_after,
                        });
                    }
                }
            }

            result.tick = if params.zero_for_one {
                tick_next.wrapping_sub(1)
            } else {
                tick_next
            };
        } else if result.sqrt_price_x96 != sqrt_start {
            if amount_remaining.is_zero() || result.sqrt_price_x96 == params.sqrt_price_limit_x96 {
                result.tick = get_tick_at_sqrt_price(result.sqrt_price_x96)?;
                break;
            }

            result.tick = get_tick_at_sqrt_price(result.sqrt_price_x96)?;
        }
    }

    let output_sent = checked_sub_u256(amount_specified, amount_remaining)?;
    let delta = make_delta_from_exact_out(params.zero_for_one, amount_in_accum, output_sent)?;

    let light = SwapSimulationResult {
        sqrt_price_x96: result.sqrt_price_x96,
        tick: result.tick,
        liquidity: result.liquidity,
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

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Returns the loosest valid price limit for a given swap direction.
///
/// Matches V4: zeroForOne must stay strictly above `MIN_SQRT_PRICE`;
/// oneForZero must stay strictly below `MAX_SQRT_PRICE`.
#[inline]
fn extreme_price_limit(zero_for_one: bool) -> U256 {
    if zero_for_one {
        MIN_SQRT_PRICE + U256::ONE
    } else {
        MAX_SQRT_PRICE - U256::ONE
    }
}

/// Validate that tick_lower < tick_upper and both are aligned to tick_spacing.
#[inline]
fn check_ticks(tick_lower: i32, tick_upper: i32, tick_spacing: i32) -> Result<(), SwapSimError> {
    if tick_lower >= tick_upper {
        return Err(SwapSimError::InvalidTick);
    }
    validate_tick_index_for_spacing(tick_lower, tick_spacing)?;
    validate_tick_index_for_spacing(tick_upper, tick_spacing)?;
    Ok(())
}

/// Unsigned token0 delta for a liquidity change between two sqrt prices.
///
/// This delegates to the optimized `sqrt_price_math` implementation so
/// liquidity modifications and swap steps share the same fast, audited math.
fn get_amount0_delta_unsigned(
    sqrt_price_a_x96: U256,
    sqrt_price_b_x96: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SwapSimError> {
    get_amount0_delta(
        sqrt_price_a_x96,
        sqrt_price_b_x96,
        U256::from(liquidity),
        round_up,
    )
    .map_err(|_| SwapSimError::AmountOverflow)
}

/// Unsigned token1 delta for a liquidity change between two sqrt prices.
///
/// This delegates to the optimized `sqrt_price_math` implementation so
/// liquidity modifications and swap steps share the same fast, audited math.
fn get_amount1_delta_unsigned(
    sqrt_price_a_x96: U256,
    sqrt_price_b_x96: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SwapSimError> {
    get_amount1_delta(
        sqrt_price_a_x96,
        sqrt_price_b_x96,
        U256::from(liquidity),
        round_up,
    )
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
    sqrt_price_a_x96: U256,
    sqrt_price_b_x96: U256,
    liquidity_delta: i128,
) -> Result<i128, SwapSimError> {
    if liquidity_delta < 0 {
        let amount = get_amount0_delta_unsigned(
            sqrt_price_a_x96,
            sqrt_price_b_x96,
            liquidity_delta.unsigned_abs(),
            false,
        )?;
        u256_magnitude_to_i128(amount, true)
    } else {
        let amount = get_amount0_delta_unsigned(
            sqrt_price_a_x96,
            sqrt_price_b_x96,
            liquidity_delta as u128,
            true,
        )?;
        u256_magnitude_to_i128(amount, false)
    }
}

fn get_amount1_delta_signed(
    sqrt_price_a_x96: U256,
    sqrt_price_b_x96: U256,
    liquidity_delta: i128,
) -> Result<i128, SwapSimError> {
    if liquidity_delta < 0 {
        let amount = get_amount1_delta_unsigned(
            sqrt_price_a_x96,
            sqrt_price_b_x96,
            liquidity_delta.unsigned_abs(),
            false,
        )?;
        u256_magnitude_to_i128(amount, true)
    } else {
        let amount = get_amount1_delta_unsigned(
            sqrt_price_a_x96,
            sqrt_price_b_x96,
            liquidity_delta as u128,
            true,
        )?;
        u256_magnitude_to_i128(amount, false)
    }
}

/// Apply a signed `liquidity_net` delta to the active liquidity.
#[inline]
fn add_liquidity_delta(liquidity: u128, delta: i128) -> Result<u128, SwapSimError> {
    if delta < 0 {
        liquidity
            .checked_sub(delta.unsigned_abs())
            .ok_or(SwapSimError::LiquidityUnderflow)
    } else {
        liquidity
            .checked_add(delta as u128)
            .ok_or(SwapSimError::LiquidityOverflow)
    }
}

/// Convert a [`SignedAmount`] to `i128`, correctly handling `i128::MIN`.
///
/// The old implementation used `abs > i128::MAX` (2^127 − 1) for *both* signs,
/// incorrectly rejecting magnitude 2^127 for negative values even though
/// `−2^127 = i128::MIN` is a valid `i128`. Negative amounts may have magnitude
/// up to 2^127 inclusive.
#[inline]
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

// ─── Validation helpers ───────────────────────────────────────────────────────

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

#[inline]
fn validate_tick_spacing(tick_spacing: i32) -> Result<(), SwapSimError> {
    if tick_spacing < MIN_TICK_SPACING || tick_spacing > MAX_TICK_SPACING {
        Err(SwapSimError::InvalidTickSpacing)
    } else {
        Ok(())
    }
}

#[inline]
fn validate_tick_range(tick: i32) -> Result<(), SwapSimError> {
    if tick < MIN_TICK || tick > MAX_TICK {
        Err(SwapSimError::InvalidTick)
    } else {
        Ok(())
    }
}

#[inline]
fn validate_tick_index_for_spacing(tick: i32, tick_spacing: i32) -> Result<(), SwapSimError> {
    validate_tick_range(tick)?;
    if tick % tick_spacing != 0 {
        Err(SwapSimError::InvalidTick)
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
    tick: i32,
    tick_spacing: i32,
    zero_for_one: bool,
) -> NextInitializedTick {
    let compressed = tick.div_euclid(tick_spacing);

    let boundary_compressed = if zero_for_one {
        // Lowest compressed tick in the current 256-bit word.
        compressed - compressed.rem_euclid(256)
    } else {
        // Highest compressed tick in the word that contains (compressed + 1).
        let next_compressed = compressed + 1;
        next_compressed + (255 - next_compressed.rem_euclid(256))
    };

    let boundary_tick = boundary_compressed
        .saturating_mul(tick_spacing)
        .clamp(MIN_TICK, MAX_TICK);

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
    limit: U256,
) -> Result<(), SwapSimError> {
    let min_price = MIN_SQRT_PRICE;
    let max_price = MAX_SQRT_PRICE;

    if state.sqrt_price_x96 < min_price || state.sqrt_price_x96 > max_price {
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

// ─── Public utilities ─────────────────────────────────────────────────────────

/// Maximum liquidity per tick for a given `tick_spacing`.
///
/// Derived from `TickMath.MAX_TICK` / number of usable compressed ticks,
/// matching V4's `Pool.tickSpacingToMaxLiquidityPerTick`.
///
/// # Panics
///
/// Panics if `tick_spacing` is outside `[MIN_TICK_SPACING, MAX_TICK_SPACING]`.
/// Use [`validate_tick_spacing`] before calling if the value is untrusted.
pub fn tick_spacing_to_max_liquidity_per_tick(tick_spacing: i32) -> u128 {
    assert!(
        tick_spacing >= MIN_TICK_SPACING && tick_spacing <= MAX_TICK_SPACING,
        "tick_spacing {tick_spacing} out of [{MIN_TICK_SPACING}, {MAX_TICK_SPACING}]"
    );
    let min_compressed = MIN_TICK.div_euclid(tick_spacing);
    let max_compressed = MAX_TICK.div_euclid(tick_spacing);
    let num_ticks = (max_compressed - min_compressed + 1) as u128;
    u128::MAX / num_ticks
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use arb_types::sqrt_price_x96_to_price;
    use proptest::prelude::*;

    // ── Shared helpers ────────────────────────────────────────────────────────

    fn ether(n: u128) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    fn sqrt_price_1_1() -> U256 {
        U256::ONE << 96
    }

    fn test_min_sqrt_price() -> U256 {
        U256::from(4_295_128_739u64)
    }

    fn basic_ticks() -> [TickEntry; 2] {
        [
            TickEntry {
                tick_idx: -120,
                liquidity_net: 1_000_000_000_000_000_000i128,
                liquidity_gross: 1_000_000_000_000_000_000u128,
            },
            TickEntry {
                tick_idx: 120,
                liquidity_net: -1_000_000_000_000_000_000i128,
                liquidity_gross: 1_000_000_000_000_000_000u128,
            },
        ]
    }

    fn make_pool(
        sqrt_price_x96: U256,
        tick: i32,
        liquidity: u128,
        fee: u32,
        tick_spacing: i32,
        ticks: impl IntoIterator<Item = TickEntry>,
    ) -> Pool {
        let tick_map = PoolTicks::from_tick_entries(ticks, tick_spacing).expect("valid ticks");

        let cache = Arc::new(PoolCache::default());

        Pool {
            state: Arc::new(RwLock::new(PoolState {
                sqrt_price_x96,
                tick,
                liquidity,
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
        pool.state.read().clone()
    }

    fn basic_pool(ticks: &[TickEntry]) -> Pool {
        make_pool(
            sqrt_price_1_1(),
            0,
            1_000_000_000_000_000_000u128,
            3_000,
            60,
            ticks.iter().cloned(),
        )
    }

    fn make_params(
        zero_for_one: bool,
        amount: SignedAmount,
        sqrt_price_limit_x96: U256,
    ) -> SwapParams {
        SwapParams {
            zero_for_one,
            amount,
            sqrt_price_limit_x96,
            #[cfg(feature = "protocol-fee")]
            protocol_fee: None,
        }
    }

    // ── Zero-amount short-circuit ─────────────────────────────────────────────

    #[test]
    fn swap_zero_amount_does_nothing() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let before = read_pool_state(&pool);

        let result = pool
            .simulate_swap(make_params(
                true,
                SignedAmount::negative(U256::ZERO),
                get_sqrt_price_at_tick(-120).unwrap(),
            ))
            .expect("zero-amount swap should succeed");

        assert_eq!(result.sqrt_price_x96, before.sqrt_price_x96);
        assert_eq!(result.tick, before.tick);
        assert_eq!(result.liquidity, before.liquidity);
        assert_eq!(result.delta, BalanceDelta::default());
    }

    // ── State mutation ────────────────────────────────────────────────────────

    #[test]
    fn swap_commits_state() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let before = read_pool_state(&pool);
        let limit = get_sqrt_price_at_tick(-120).unwrap();

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
        let limit = get_sqrt_price_at_tick(-120).unwrap();

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
        let params = make_params(
            true,
            SignedAmount::negative(ether(1)),
            get_sqrt_price_at_tick(-120).unwrap(),
        );

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
            get_sqrt_price_at_tick(-120).unwrap(),
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

    // ── V4 caller / PoolManager sign convention ───────────────────────────────

    #[test]
    fn zero_for_one_exact_in_delta_signs() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let limit = get_sqrt_price_at_tick(-120).unwrap();

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
        let limit = get_sqrt_price_at_tick(120).unwrap();

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
    // ── Price movement direction ───────────────────────────────────────────────

    #[test]
    fn zero_for_one_price_moves_down() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let limit = get_sqrt_price_at_tick(-120).unwrap();

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
        let limit = get_sqrt_price_at_tick(120).unwrap();

        let before_price = read_pool_state(&pool).sqrt_price_x96;
        let result = pool
            .simulate_swap(make_params(false, SignedAmount::negative(ether(1)), limit))
            .unwrap();

        assert!(result.sqrt_price_x96 >= before_price, "price must not fall");
        assert!(result.sqrt_price_x96 <= limit, "price must not cross limit");
    }

    // ── amount_in / amount_out accessor correctness ───────────────────────────

    #[test]
    fn amount_in_and_out_are_consistent_with_raw_delta() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let limit = get_sqrt_price_at_tick(-120).unwrap();

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

    // ── direction-aware delta helper sign guards ─────────────────────────────

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

    // ── signed_amount_to_i128 boundary ───────────────────────────────────────

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

    // ── Exact-output ──────────────────────────────────────────────────────────

    #[test]
    fn exact_out_never_exceeds_requested() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        let requested = 1_000_000_000_000u128;

        let result = pool
            .simulate_swap(make_params(
                true,
                SignedAmount::positive(U256::from(requested)),
                get_sqrt_price_at_tick(-120).unwrap(),
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
                get_sqrt_price_at_tick(-240).unwrap(),
            ))
            .unwrap();

        // 틱 -120을 건넜으므로 crossings가 비어있으면 안 됨
        assert_eq!(result.crossings.len(), 1);
        assert_eq!(result.crossings[0].tick, -120);

        // cumulative_input은 0보다 크고 전체 입력보다 작아야 함
        let total_input = U256::from(delta_amount_in(&result.delta, true));
        assert!(result.crossings[0].cumulative_input > U256::ZERO);
        assert!(result.crossings[0].cumulative_input <= total_input);

        // 크로싱 후 유동성은 0이어야 함 (범위 아래)
        assert_eq!(result.crossings[0].liquidity_after, 0);
    }

    #[test]
    fn crossings_empty_when_no_tick_crossed() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);

        // 틱을 건너지 않을 작은 스왑
        let result = pool
            .simulate_swap_full(make_params(
                true,
                SignedAmount::negative(U256::from(1_000u128)),
                get_sqrt_price_at_tick(-1).unwrap(),
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
                get_sqrt_price_at_tick(-240).unwrap(),
            ))
            .unwrap();

        // exact-output에서 crossings의 cumulative_input은 모두 0
        for c in &result.crossings {
            assert_eq!(c.cumulative_input, U256::ZERO);
        }
    }

    // ── Tick crossing ─────────────────────────────────────────────────────────

    fn one_range_pool(liq: u128) -> Pool {
        make_pool(
            get_sqrt_price_at_tick(0).unwrap(),
            0,
            liq,
            3_000,
            60,
            [
                TickEntry {
                    tick_idx: -120,
                    liquidity_net: liq as i128,
                    liquidity_gross: liq,
                },
                TickEntry {
                    tick_idx: 120,
                    liquidity_net: -(liq as i128),
                    liquidity_gross: liq,
                },
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
                get_sqrt_price_at_tick(-240).unwrap(),
            ))
            .expect("swap should succeed");

        assert!(result.tick <= -120, "must have crossed tick -120");
        assert_eq!(result.liquidity, 0, "no liquidity below range");
    }

    #[test]
    fn crossing_tick_upward_drains_liquidity() {
        let liq = 500_000_000_000_000_000u128;
        let pool = one_range_pool(liq);

        let result = pool
            .simulate_swap(make_params(
                false,
                SignedAmount::negative(ether(100)),
                get_sqrt_price_at_tick(240).unwrap(),
            ))
            .expect("swap should succeed");

        assert!(result.tick >= 120, "must have crossed tick 120");
        assert_eq!(result.liquidity, 0, "no liquidity above range");
    }

    // ── modify_liquidity ──────────────────────────────────────────────────────

    #[test]
    fn add_liquidity_inside_range() {
        let pool = make_pool(sqrt_price_1_1(), 0, 0, 3_000, 60, []);
        let liq = 1_000_000_000_000u128;

        let result = pool
            .modify_liquidity(ModifyLiquidityParams::new(-120, 120, liq as i128))
            .expect("add should succeed");

        assert_eq!(result.liquidity, liq);
        // Liquidity delta is pool-balance perspective: pool receives tokens when LP adds.
        assert!(result.delta.amount0 > 0, "pool receives token0");
        assert!(result.delta.amount1 > 0, "pool receives token1");
        assert!(result.flipped_lower && result.flipped_upper);

        let ticks = pool.ticks.read();
        assert_eq!(ticks.get(-120).unwrap().liquidity_net, liq as i128);
        assert_eq!(ticks.get(120).unwrap().liquidity_net, -(liq as i128));
    }

    #[test]
    fn add_liquidity_below_range_only_token0() {
        let pool = make_pool(
            get_sqrt_price_at_tick(-240).unwrap(),
            -240,
            0,
            3_000,
            60,
            [],
        );

        let result = pool
            .modify_liquidity(ModifyLiquidityParams::new(-120, 120, 1_000_000_000_000i128))
            .expect("add should succeed");

        assert_eq!(result.liquidity, 0, "out-of-range position");

        // Price below range: adding liquidity requires only token0; pool receives it.
        assert!(result.delta.amount0 > 0, "pool receives token0");
        assert_eq!(result.delta.amount1, 0);
    }

    #[test]
    fn remove_liquidity_clears_ticks() {
        let pool = make_pool(sqrt_price_1_1(), 0, 0, 3_000, 60, []);
        let liq = 1_000_000_000_000i128;

        pool.modify_liquidity(ModifyLiquidityParams::new(-120, 120, liq))
            .unwrap();

        let result = pool
            .modify_liquidity(ModifyLiquidityParams::new(-120, 120, -liq))
            .expect("remove should succeed");

        assert_eq!(result.liquidity, 0);
        // Removing liquidity means the pool sends tokens back to the LP.
        assert!(result.delta.amount0 < 0, "pool sends token0 back");
        assert!(result.delta.amount1 < 0, "pool sends token1 back");
        assert!(result.flipped_lower && result.flipped_upper);

        let ticks = pool.ticks.read();
        assert!(ticks.get(-120).is_none());
        assert!(ticks.get(120).is_none());
    }

    // ── Validation ────────────────────────────────────────────────────────────

    #[test]
    fn rejects_out_of_range_state_tick() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        pool.state.write().tick = MIN_TICK - 1;

        assert_eq!(
            pool.simulate_swap(make_params(
                true,
                SignedAmount::negative(ether(1)),
                get_sqrt_price_at_tick(-60).unwrap(),
            ))
            .unwrap_err(),
            SwapSimError::InvalidTick
        );
    }

    #[test]
    fn rejects_out_of_range_tick_entry() {
        assert_eq!(
            PoolTicks::from_tick_entries(
                [TickEntry {
                    tick_idx: MIN_TICK - 1,
                    liquidity_net: 0,
                    liquidity_gross: 1
                }],
                60,
            )
            .unwrap_err(),
            SwapSimError::InvalidTick
        );
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
                get_sqrt_price_at_tick(-60).unwrap(),
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
                get_sqrt_price_at_tick(-60).unwrap(),
            ))
            .unwrap_err(),
            SwapSimError::FeeTooLarge
        );
    }

    #[test]
    fn rejects_zero_tick_spacing() {
        let ticks = basic_ticks();
        let mut pool = basic_pool(&ticks);
        pool.tick_spacing = 0;

        assert_eq!(
            pool.simulate_swap(make_params(
                true,
                SignedAmount::negative(ether(1)),
                get_sqrt_price_at_tick(-60).unwrap(),
            ))
            .unwrap_err(),
            SwapSimError::InvalidTickSpacing
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
                get_sqrt_price_at_tick(1).unwrap(),
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
                get_sqrt_price_at_tick(-1).unwrap(),
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
                test_min_sqrt_price() - U256::ONE,
            ))
            .unwrap_err(),
            SwapSimError::PriceLimitOutOfBounds
        );
    }

    #[test]
    fn rejects_invalid_pool_sqrt_price() {
        let ticks = basic_ticks();
        let pool = basic_pool(&ticks);
        pool.state.write().sqrt_price_x96 = test_min_sqrt_price() - U256::ONE;

        assert_eq!(
            pool.simulate_swap(make_params(
                true,
                SignedAmount::negative(ether(1)),
                test_min_sqrt_price(),
            ))
            .unwrap_err(),
            SwapSimError::InvalidPoolSqrtPrice
        );
    }

    // ── next_initialized_tick ─────────────────────────────────────────────────

    fn tick_pool_for_search() -> PoolTicks {
        PoolTicks::from_tick_entries(
            [
                TickEntry {
                    tick_idx: -240,
                    liquidity_net: 0,
                    liquidity_gross: 1,
                },
                TickEntry {
                    tick_idx: 0,
                    liquidity_net: 0,
                    liquidity_gross: 1,
                },
                TickEntry {
                    tick_idx: 240,
                    liquidity_net: 0,
                    liquidity_gross: 1,
                },
            ],
            60,
        )
        .expect("valid ticks")
    }

    #[test]
    fn next_tick_down_returns_tick_at_current() {
        let ticks = tick_pool_for_search();
        let guard = ticks.read();
        let next = guard.next_initialized_tick(0, true);
        assert_eq!(next.tick_next, 0);
        assert!(next.initialized);
    }

    #[test]
    fn next_tick_up_skips_current() {
        let ticks = tick_pool_for_search();
        let guard = ticks.read();
        let next = guard.next_initialized_tick(0, false);
        assert_eq!(next.tick_next, 240);
        assert!(next.initialized);
    }

    #[test]
    fn next_tick_returns_sentinel_when_empty() {
        let ticks = PoolTicks::from_tick_entries(
            [TickEntry {
                tick_idx: 120,
                liquidity_net: 0,
                liquidity_gross: 1,
            }],
            60,
        )
        .unwrap();
        let guard = ticks.read();

        let down = guard.next_initialized_tick(50, true);
        assert_eq!(down.tick_next, MIN_TICK);
        assert!(!down.initialized);

        let up = guard.next_initialized_tick(120, false);
        assert_eq!(up.tick_next, MAX_TICK);
        assert!(!up.initialized);
    }

    // ── tick_spacing_to_max_liquidity_per_tick ────────────────────────────────

    #[test]
    fn max_liquidity_matches_formula() {
        for &ts in &[1i32, 10, 60, 200, 32_767] {
            let result = tick_spacing_to_max_liquidity_per_tick(ts);
            let min_c = MIN_TICK.div_euclid(ts);
            let max_c = MAX_TICK.div_euclid(ts);
            let num_ticks = (max_c - min_c + 1) as u128;
            assert_eq!(
                result,
                u128::MAX / num_ticks,
                "mismatch at tick_spacing={ts}"
            );
        }
    }

    // ── Property-based tests ──────────────────────────────────────────────────

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
                TickEntry { tick_idx: -600, liquidity_net:  liq as i128, liquidity_gross: liq },
                TickEntry { tick_idx:  600, liquidity_net: -(liq as i128), liquidity_gross: liq },
            ];
            let pool = make_pool(get_sqrt_price_at_tick(0).unwrap(), 0, liq, fee, 60, ticks);

            let limit_tick = if zero_for_one { -limit_tick_offset } else { limit_tick_offset };
            let limit = get_sqrt_price_at_tick(limit_tick).unwrap();
            let amount = if exact_in {
                SignedAmount::negative(U256::from(amount_raw))
            } else {
                SignedAmount::positive(U256::from(amount_raw))
            };

            let result = pool
                .simulate_swap(make_params(zero_for_one, amount, limit))
                .expect("swap should succeed");

            let before_price = get_sqrt_price_at_tick(0).unwrap();
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
                TickEntry { tick_idx: -600, liquidity_net:  liq as i128, liquidity_gross: liq },
                TickEntry { tick_idx:  600, liquidity_net: -(liq as i128), liquidity_gross: liq },
            ];
            let pool = make_pool(get_sqrt_price_at_tick(0).unwrap(), 0, liq, 3_000, 60, ticks);

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
                TickEntry { tick_idx: -600, liquidity_net:  liq as i128, liquidity_gross: liq },
                TickEntry { tick_idx:  600, liquidity_net: -(liq as i128), liquidity_gross: liq },
            ];
            let pool = make_pool(get_sqrt_price_at_tick(0).unwrap(), 0, liq, 3_000, 60, ticks);

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
