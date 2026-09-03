use alloy::primitives::{Address, FixedBytes, I256};
use ruint::aliases::U256;

use crate::{
    core::types::tick_slab_indexer::TickSlabIndexer,
    v4::{BalanceDelta, Fee, Liquidity, SqrtPriceX96, TickIndex, TickInfo},
};

/// Parameters for one Uniswap v4-style swap.
///
/// Mirrors `IPoolManager.SwapParams`: direction, signed input/output amount,
/// and the caller's sqrt-price safety limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapParams {
    /// `true`  → sell token0, buy token1 (price moves **down**).
    /// `false` → sell token1, buy token0 (price moves **up**).
    pub zero_for_one: bool,

    /// Signed swap amount, matching v4 `amountSpecified` semantics.
    ///
    /// * negative → exact-input; absolute value is the input amount.
    /// * positive → exact-output; absolute value is the desired output amount.
    pub amount_specified: I256,

    /// Worst acceptable sqrt price after the swap, in Q64.96.
    ///
    /// * `zero_for_one = true`  → must be in `(MIN_SQRT_PRICE, current_sqrt_price)`.
    /// * `zero_for_one = false` → must be in `(current_sqrt_price, MAX_SQRT_PRICE)`.
    pub sqrt_price_limit_x96: SqrtPriceX96,
}

impl SwapParams {
    /// Constructs a swap with the loosest valid price limit for the direction.
    pub fn new(zero_for_one: bool, amount: I256) -> Self {
        Self {
            zero_for_one,
            amount_specified: amount,
            sqrt_price_limit_x96: SqrtPriceX96::extreme_price_limit(zero_for_one),
        }
    }
}

/// Parameters for a Uniswap v4-style liquidity update.
///
/// Mirrors the pool-facing portion of `IPoolManager.ModifyLiquidityParams`.
/// `info` carries the owner/salt pair only when the caller wants position-level
/// fee accounting in addition to the pool-level liquidity change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifyLiquidityParams {
    /// Inclusive lower tick of the liquidity range.
    pub tick_lower: TickIndex,

    /// Exclusive upper tick of the liquidity range.
    pub tick_upper: TickIndex,

    /// Signed liquidity delta. Positive adds liquidity, negative removes it.
    pub liquidity_delta: i128,

    /// Account or manager address that owns the position.
    pub owner: Address,

    /// User-provided discriminator for otherwise identical owner/range pairs.
    pub salt: FixedBytes<32>,
}

/// Output of a liquidity position update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifyLiquidityResult {
    /// Principal token delta produced by the liquidity change.
    pub delta: BalanceDelta,

    /// Fees realized from position accounting during the update.
    pub fee_delta: BalanceDelta,
}

/// Output of a successful simulated swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapResult {
    /// Signed token deltas in V4's caller / PoolManager `BalanceDelta` convention.
    ///
    /// Use [`BalanceDelta::amount_in`] / [`BalanceDelta::amount_out`] for
    /// direction-aware unsigned magnitudes.
    pub swap_delta: BalanceDelta,

    /// Protocol fee collected from the input token, in raw token units.
    pub amount_to_protocol: U256,

    /// Effective swap fee used for this direction, including protocol fee.
    pub swap_fee: Fee,

    /// Pool sqrt price after the swap, in Q64.96.
    pub sqrt_price_x96: SqrtPriceX96,

    /// Pool tick after the swap.
    pub tick: TickIndex,

    /// Active liquidity after the swap. May differ from the input value
    /// if tick boundaries were crossed.
    pub liquidity: Liquidity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepComputations {
    /// Pool sqrt price at the beginning of the current step.
    pub sqrt_price_start_x96: SqrtPriceX96,

    /// Next initialized tick payload in the swap direction.
    pub next_tick_info: TickInfo,

    /// Slab indexer for `next_tick_info`.
    pub next_tick_indexer: TickSlabIndexer,

    /// Whether the next step target is an initialized tick boundary.
    pub initialized: bool,

    /// Sqrt price target for the next step, bounded by the caller's price limit.
    pub sqrt_price_next_x96: SqrtPriceX96,

    /// Input amount consumed during this step, before protocol-fee splitting.
    pub amount_in: U256,

    /// Output amount produced during this step.
    pub amount_out: U256,

    /// LP fee amount retained after subtracting any protocol fee.
    pub fee_amount: U256,

    /// Fee-growth accumulator for the input token, updated as the swap advances.
    pub fee_growth_global_x128: U256,
}

impl StepComputations {
    pub const DEFAULT: Self = StepComputations {
        sqrt_price_start_x96: SqrtPriceX96::MIN,
        next_tick_info: TickInfo::DEFAULT,
        next_tick_indexer: TickSlabIndexer::from_tick(TickIndex::MIN, 1),
        initialized: false,
        sqrt_price_next_x96: SqrtPriceX96::MIN,
        amount_in: U256::ZERO,
        amount_out: U256::ZERO,
        fee_amount: U256::ZERO,
        fee_growth_global_x128: U256::ZERO,
    };
}

/// Compact swap simulation result for callers that only need final swap state.
///
/// This result shape keeps the hot quote path focused on final pool state,
/// token deltas, fee growth, and protocol fee collection, without carrying
/// debug-only crossing metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapSimulationResult {
    /// Pool sqrt price after the simulated swap, in Q64.96.
    pub sqrt_price_x96: SqrtPriceX96,

    /// Pool tick after the simulated swap.
    pub tick: TickIndex,

    /// Active liquidity after the simulated swap.
    pub liquidity: Liquidity,

    /// All-time LP fee growth per unit of liquidity in token0 after the swap.
    pub fee_growth_global0_x128: U256,

    /// All-time LP fee growth per unit of liquidity in token1 after the swap.
    pub fee_growth_global1_x128: U256,

    /// Signed token deltas in V4's caller / PoolManager `BalanceDelta` convention.
    pub delta: BalanceDelta,

    /// Protocol fee collected in the input token, in raw token units.
    pub protocol_fee_amount: u128,
}
