use alloy::primitives::I256;
use ruint::aliases::U256;

use crate::{
    core::types::tick_slab_indexer::TickSlabIndexer,
    v4::{BalanceDelta, Fee, Liquidity, SqrtPriceX96, TickIndex, TickInfo},
};

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
    pub amount: I256,

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
    pub fn new(zero_for_one: bool, amount: I256) -> Self {
        Self {
            zero_for_one,
            amount,
            sqrt_price_limit_x96: SqrtPriceX96::extreme_price_limit(zero_for_one),
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

/// Output of a liquidity position update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifyLiquidityResult {
    pub delta: BalanceDelta,
    pub fee_delta: BalanceDelta,
}

/// Output of a successful simulated swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapResult {
    /// Signed token deltas in V4's caller / PoolManager `BalanceDelta` convention.
    ///
    /// Use [`delta_amount_in`] / [`delta_amount_out`] for
    /// direction-aware unsigned magnitudes.
    pub swap_delta: BalanceDelta,

    pub amount_to_protocol: U256,

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
    // the price at the beginning of the step
    pub sqrt_price_start_x96: SqrtPriceX96,
    // the next tick to swap to from the current tick in the swap direction
    pub next_tick_info: TickInfo,
    pub next_tick_indexer: TickSlabIndexer,
    // whether tickNext is initialized or not
    pub initialized: bool,
    // sqrt(price) for the next tick (1/0)
    pub sqrt_price_next_x96: SqrtPriceX96,
    // how much is being swapped in in this step
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
    // how much is being swapped out
    // how much fee is being paid in
    // the global fee growth of the input token. updated in storage at the end of swap
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
