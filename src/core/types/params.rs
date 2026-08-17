use ruint::aliases::U256;

use crate::v4::{BalanceDelta, Liquidity, SignedAmount, SqrtPriceX96, TickCrossInfo, TickIndex};

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
    pub fn into_full(self, crossings: Vec<TickCrossInfo>) -> FullSwapResult {
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
