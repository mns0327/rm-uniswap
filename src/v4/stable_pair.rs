//! Port of Uniswap Labs' `StablePairHook`, a dynamic-fee hook for pools of
//! two assets expected to trade at the same price (e.g. USDC/USDT).
//!
//! Onchain it is the most used V4 dynamic-fee hook on Ethereum mainnet that
//! does not return deltas: ERC1967 proxy
//! `0x0000113dcf4add69999fad8f20f2b63f979bfcc0`, implementation
//! `0x5f216d21b1c81346938eddc1df7e0111a0f64000`.
//!
//! The fee keeps pre-impact prices pinned to an "optimal range" around a
//! reference price; outside it, a per-block exponentially decaying fee is
//! charged only to swaps moving the price back toward the reference. The
//! arithmetic, including solady's `expWad`/`lnWad`, is ported bit-for-bit so
//! fees match the contract exactly.
//!
//! The hook reads [`HookEnv::block_number`] and the pre-swap pool price from
//! [`HookEnv::pool_state`]; with [`SinglePoolManager`] set the block through
//! [`SinglePoolManager::set_block_number`].
//!
//! [`SinglePoolManager`]: crate::v4::positions::SinglePoolManager
//! [`SinglePoolManager::set_block_number`]: crate::v4::positions::SinglePoolManager::set_block_number

use std::str::FromStr;

use alloy::primitives::{Address, I256};
use ruint::aliases::U256;

use crate::{
    BalanceDelta, Error, Fee, ModifyLiquidityParams, SqrtPriceX96, SwapParams,
    core::types::hooks::{HookEnv, Hooks, HooksImpl},
    v4::{BeforeSwapDelta, PoolKey},
};

const ONE_E6: u64 = 1_000_000;
const ONE_E12: u64 = 1_000_000_000_000;
/// Sentinel: no decaying fee (inside optimal range).
const UNDEFINED_DECAYING_FEE_E12: u64 = ONE_E12 + 1;
const MAX_FEE_E6: u64 = ONE_E6 - 2;
const MAX_TARGET_MULTIPLIER: u64 = 100;
const MAX_OPTIMAL_FEE_E6: u64 = 10_000;

/// Per-pool fee parameters, as returned by the contract's `feeConfig(poolId)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StableFeeConfig {
    /// Per-block decay factor in Q24; nonzero and below `2^24`.
    pub k: u32,
    /// Half-width of the optimal range in pips; at most `10_000` (1%).
    pub optimal_fee_e6: u32,
    /// How far the decay target drops below the far-boundary fee, `0..=100`.
    pub target_multiplier: u8,
    /// Reference sqrt price, Q64.96.
    pub reference_sqrt_price_x96: U256,
}

/// Per-pool fee state, as returned by the contract's `feeState(poolId)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StableFeeState {
    /// Decaying fee in 1e12 precision, or `1e12 + 1` inside the optimal range.
    pub decaying_fee_e12: u64,
    /// Cached start-of-block AMM sqrt price; `0` forces a fresh read.
    pub sqrt_amm_price_x96: U256,
    /// Block the state was last written in.
    pub block_number: u64,
}

impl StableFeeState {
    /// State right after pool initialization or a config update at `block_number`.
    pub const fn reset(block_number: u64) -> Self {
        Self {
            decaying_fee_e12: UNDEFINED_DECAYING_FEE_E12,
            sqrt_amm_price_x96: U256::ZERO,
            block_number,
        }
    }
}

/// [`Hooks`] implementation of `StablePairHook` for one pool.
#[derive(Debug, Clone)]
pub struct StablePairHook {
    config: StableFeeConfig,
    state: StableFeeState,
}

impl StablePairHook {
    /// Builds the hook from a config and its current state.
    ///
    /// Returns [`Error::InvalidHookConfig`] for configs the contract rejects.
    pub fn new(config: StableFeeConfig, state: StableFeeState) -> Result<Self, Error> {
        validate_config(&config)?;
        Ok(Self { config, state })
    }

    pub fn config(&self) -> &StableFeeConfig {
        &self.config
    }

    pub fn state(&self) -> &StableFeeState {
        &self.state
    }

    /// Returns the LP fee in pips for a swap in `zero_for_one` direction and
    /// commits the per-block fee state like the contract's `_beforeSwap`.
    pub fn before_swap_fee(&mut self, env: &HookEnv<'_>, zero_for_one: bool) -> u32 {
        let is_new_block =
            env.block_number > self.state.block_number || self.state.sqrt_amm_price_x96.is_zero();
        let sqrt_amm_price_x96 = if is_new_block {
            env.pool_state.sqrt_price_x96.as_u256()
        } else {
            self.state.sqrt_amm_price_x96
        };

        let (lp_fee_e12, decaying_fee_e12) = self.get_fee(
            sqrt_amm_price_x96,
            is_new_block,
            zero_for_one,
            env.block_number,
        );

        if is_new_block {
            self.state = StableFeeState {
                decaying_fee_e12,
                sqrt_amm_price_x96,
                block_number: env.block_number,
            };
        }
        to_fee_e6(lp_fee_e12)
    }
    fn get_fee(
        &self,
        sqrt_amm_price_x96: U256,
        is_new_block: bool,
        zero_for_one: bool,
        block_number: u64,
    ) -> (U256, u64) {
        let sqrt_reference_price_x96 = self.config.reference_sqrt_price_x96;
        let optimal_fee_e6 = self.config.optimal_fee_e6 as u64;
        let price_ratio_x96 = price_ratio_x96(sqrt_amm_price_x96, sqrt_reference_price_x96);
        let close_boundary_fee_e12 = close_boundary_fee(price_ratio_x96, optimal_fee_e6);
        let amm_price_below_rp = sqrt_amm_price_x96 < sqrt_reference_price_x96;

        if close_boundary_fee_e12 <= 0 {
            let lp_fee_e12 = inside_optimal_range_fee(
                price_ratio_x96,
                optimal_fee_e6,
                amm_price_below_rp,
                zero_for_one,
            );
            return (lp_fee_e12, UNDEFINED_DECAYING_FEE_E12);
        }

        let decaying_fee_e12 = if is_new_block {
            self.decaying_fee(
                sqrt_amm_price_x96,
                close_boundary_fee_e12 as u64,
                far_boundary_fee(price_ratio_x96, optimal_fee_e6),
                amm_price_below_rp,
                block_number,
            )
        } else {
            self.state.decaying_fee_e12
        };

        // Price moving further from reference pays nothing; otherwise the decaying fee.
        let lp_fee_e12 = if amm_price_below_rp == zero_for_one {
            U256::ZERO
        } else {
            U256::from(decaying_fee_e12)
        };
        (lp_fee_e12, decaying_fee_e12)
    }

    fn decaying_fee(
        &self,
        sqrt_amm_price_x96: U256,
        close_boundary_fee_e12: u64,
        far_boundary_fee_e12: u64,
        amm_price_below_rp: bool,
        block_number: u64,
    ) -> u64 {
        let previous_sqrt = self.state.sqrt_amm_price_x96;
        let previous_fee = self.state.decaying_fee_e12;
        let reference = self.config.reference_sqrt_price_x96;

        let mut decay_start_fee_e12 = if previous_fee == UNDEFINED_DECAYING_FEE_E12
            || (previous_sqrt < reference) != amm_price_below_rp
        {
            far_boundary_fee_e12
        } else if amm_price_below_rp == (sqrt_amm_price_x96 < previous_sqrt) {
            adjust_previous_fee_for_price_movement(
                price_ratio_x96(sqrt_amm_price_x96, previous_sqrt),
                previous_fee,
            )
        } else if previous_fee > far_boundary_fee_e12 {
            far_boundary_fee_e12
        } else {
            previous_fee
        };

        let target_fee_e12 = far_boundary_fee_e12
            - close_boundary_fee_e12 * self.config.target_multiplier as u64 / MAX_TARGET_MULTIPLIER;
        decay_start_fee_e12 = decay_start_fee_e12.max(target_fee_e12);

        let blocks_passed = block_number.saturating_sub(self.state.block_number);
        decaying_fee(
            target_fee_e12,
            decay_start_fee_e12,
            self.config.k as u64,
            blocks_passed,
        )
    }
}

fn validate_config(config: &StableFeeConfig) -> Result<(), Error> {
    // Optimal-range bounds must stay inside V4's sqrt price range at the
    // maximum optimal fee: [MIN_SQRT_PRICE / sqrt(1 - f), MAX_SQRT_PRICE * sqrt(1 - f)).
    let sqrt_one_minus_max_fee_e6 = U256::from(((ONE_E6 - MAX_OPTIMAL_FEE_E6) * ONE_E6).isqrt());
    let one_e6 = U256::from(ONE_E6);
    let min_reference = (SqrtPriceX96::MIN.as_u256() * one_e6 + sqrt_one_minus_max_fee_e6
        - U256::ONE)
        / sqrt_one_minus_max_fee_e6;
    let max_reference = SqrtPriceX96::MAX.as_u256() * sqrt_one_minus_max_fee_e6 / one_e6;

    let valid = config.k != 0
        && config.k < 1 << 24
        && config.optimal_fee_e6 as u64 <= MAX_OPTIMAL_FEE_E6
        && config.target_multiplier as u64 <= MAX_TARGET_MULTIPLIER
        && config.reference_sqrt_price_x96 >= min_reference
        && config.reference_sqrt_price_x96 < max_reference;
    valid.then_some(()).ok_or(Error::InvalidHookConfig)
}

fn q96() -> U256 {
    U256::ONE << 96
}

fn to_fee_e6(fee_e12: U256) -> u32 {
    let fee_e6 = fee_e12.div_ceil(U256::from(ONE_E6));
    fee_e6.min(U256::from(MAX_FEE_E6)).to::<u32>()
}

fn price_ratio_x96(a: U256, b: U256) -> U256 {
    let q48 = U256::ONE << 48;
    let sqrt_ratio = if a < b { a * q48 / b } else { b * q48 / a };
    sqrt_ratio * sqrt_ratio
}

fn close_boundary_fee(price_ratio_x96: U256, optimal_fee_e6: u64) -> i128 {
    let term = U256::from(ONE_E12) * price_ratio_x96 * U256::from(ONE_E6)
        / U256::from(ONE_E6 - optimal_fee_e6)
        / q96();
    ONE_E12 as i128 - term.to::<i128>()
}

fn inside_optimal_range_fee(
    price_ratio_x96: U256,
    optimal_fee_e6: u64,
    amm_price_below_rp: bool,
    sells_zero_for_one: bool,
) -> U256 {
    let scaled = U256::from(ONE_E12) * U256::from(ONE_E6 - optimal_fee_e6);
    let term = if amm_price_below_rp == sells_zero_for_one {
        scaled * q96() / price_ratio_x96 / U256::from(ONE_E6)
    } else {
        scaled * price_ratio_x96 / q96() / U256::from(ONE_E6)
    };
    U256::from(ONE_E12) - term
}

fn far_boundary_fee(price_ratio_x96: U256, optimal_fee_e6: u64) -> u64 {
    let term = U256::from(ONE_E12) * U256::from(ONE_E6 - optimal_fee_e6) * price_ratio_x96
        / q96()
        / U256::from(ONE_E6);
    ONE_E12 - term.to::<u64>()
}

fn adjust_previous_fee_for_price_movement(price_ratio_x96: U256, previous_fee_e12: u64) -> u64 {
    ONE_E12 - (price_ratio_x96 * U256::from(ONE_E12 - previous_fee_e12) / q96()).to::<u64>()
}

fn decaying_fee(target_fee_e12: u64, previous_fee_e12: u64, k: u64, blocks_passed: u64) -> u64 {
    let k = U256::from(k);
    let factor_x24 = match blocks_passed {
        0 => U256::ONE << 24,
        1 => k,
        2 => (k * k) >> 24,
        3 => (k * k * k) >> 48,
        4 => ((k * k) * (k * k)) >> 72,
        _ => {
            let exponent =
                I256::from_raw((U256::from(derive_log_k(k)) << 24) * U256::from(blocks_passed));
            (exp_wad(-exponent).into_raw() << 24) / U256::from(10u64.pow(18))
        }
    };
    let decay: U256 = (factor_x24 * U256::from(previous_fee_e12 - target_fee_e12)) >> 24;
    target_fee_e12 + decay.to::<u64>()
}

fn derive_log_k(k: U256) -> u64 {
    let k_wad = (k * U256::from(10u64.pow(18))) >> 24;
    let ln_k = ln_wad(I256::from_raw(k_wad));
    let log_k: U256 = ((-ln_k).into_raw() + U256::from((1u64 << 24) - 1)) >> 24;
    log_k.to::<u64>()
}

fn int(value: &str) -> I256 {
    I256::from_str(value).unwrap()
}

fn sar(value: I256, shift: usize) -> I256 {
    value.asr(shift)
}

/// solady `FixedPointMathLib.expWad`, in EVM wrapping arithmetic.
fn exp_wad(x: I256) -> I256 {
    if x <= int("-41446531673892822313") {
        return I256::ZERO;
    }
    assert!(x < int("135305999368893231589"), "ExpOverflow");

    let x = I256::from_raw(x.into_raw() << 78) / int("3814697265625");
    let ln2 = int("54916777467707473351141471128");
    let k = sar(
        I256::from_raw(x.into_raw() << 96) / ln2 + I256::from_raw(U256::ONE << 95),
        96,
    );
    let x = x.wrapping_sub(k.wrapping_mul(ln2));

    let mut y = x.wrapping_add(int("1346386616545796478920950773328"));
    y = sar(y.wrapping_mul(x), 96).wrapping_add(int("57155421227552351082224309758442"));
    let mut p = y
        .wrapping_add(x)
        .wrapping_sub(int("94201549194550492254356042504812"));
    p = sar(p.wrapping_mul(y), 96).wrapping_add(int("28719021644029726153956944680412240"));
    p = p.wrapping_mul(x).wrapping_add(I256::from_raw(
        int("4385272521454847904659076985693276").into_raw() << 96,
    ));

    let mut q = x.wrapping_sub(int("2855989394907223263936484059900"));
    for c in [
        "50020603652535783019961831881945",
        "-533845033583426703283633433725380",
        "3604857256930695427073651918091429",
        "-14423608567350463180887372962807573",
        "26449188498355588339934803723976023",
    ] {
        q = sar(q.wrapping_mul(x), 96).wrapping_add(int(c));
    }

    let r = p / q;
    let shift = (195 - k.as_i64()) as usize;
    I256::from_raw(
        r.into_raw().wrapping_mul(
            U256::from_str("3822833074963236453042738258902158003155416615667").unwrap(),
        ) >> shift,
    )
}

/// solady `FixedPointMathLib.lnWad`, in EVM wrapping arithmetic.
fn ln_wad(x: I256) -> I256 {
    assert!(x > I256::ZERO, "LnWadUndefined");
    let xu = x.into_raw();

    let mut r = ((xu > U256::from(u128::MAX)) as usize) << 7;
    r |= (((xu >> r) > U256::from(u64::MAX)) as usize) << 6;
    r |= (((xu >> r) > U256::from(u32::MAX)) as usize) << 5;
    r |= (((xu >> r) > U256::from(u16::MAX)) as usize) << 4;
    r |= (((xu >> r) > U256::from(u8::MAX)) as usize) << 3;
    let magic = U256::from(0x8421084210842108cc6318c6db6d54be_u128);
    let index = ((magic >> (xu >> r).to::<usize>()) & U256::from(0x1f)).to::<usize>();
    let table =
        U256::from_str("0xf8f9f9faf9fdfafbf9fdfcfdfafbfcfef9fafdfafcfcfbfefafafcfbffffffff")
            .unwrap();
    r ^= ((table >> (8 * (31 - index))) & U256::from(0xff)).to::<usize>();

    let x = I256::from_raw((xu << r) >> 159);

    let mut p = int("3273285459638523848632254066296").wrapping_add(x);
    p = sar(p.wrapping_mul(x), 96).wrapping_add(int("24828157081833163892658089445524"));
    p = sar(p.wrapping_mul(x), 96).wrapping_add(int("43456485725739037958740375743393"));
    p = sar(p.wrapping_mul(x), 96).wrapping_sub(int("11111509109440967052023855526967"));
    p = sar(p.wrapping_mul(x), 96).wrapping_sub(int("45023709667254063763336534515857"));
    p = sar(p.wrapping_mul(x), 96).wrapping_sub(int("14706773417378608786704636184526"));
    p = p.wrapping_mul(x).wrapping_sub(I256::from_raw(
        int("795164235651350426258249787498").into_raw() << 96,
    ));

    let mut q = int("5573035233440673466300451813936").wrapping_add(x);
    for c in [
        "71694874799317883764090561454958",
        "283447036172924575727196451306956",
        "401686690394027663651624208769553",
        "204048457590392012362485061816622",
        "31853899698501571402653359427138",
        "909429971244387300277376558375",
    ] {
        q = int(c).wrapping_add(sar(x.wrapping_mul(q), 96));
    }

    let mut p = p / q;
    p = int("1677202110996718588342820967067443963516166").wrapping_mul(p);
    p = int("16597577552685614221487285958193947469193820559219878177908093499208371")
        .wrapping_mul(I256::try_from(159 - r as i64).unwrap())
        .wrapping_add(p);
    p = int("600920179829731861736702779321621459595472258049074101567377883020018308")
        .wrapping_add(p);
    sar(p, 174)
}

impl HooksImpl for StablePairHook {}

impl Hooks for StablePairHook {
    fn before_add_liquidity(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: &ModifyLiquidityParams,
        _hook_data: &[u8],
    ) -> Result<(), Error> {
        Ok(())
    }

    fn after_add_liquidity(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: &ModifyLiquidityParams,
        _delta: BalanceDelta,
        _fees_accrued: BalanceDelta,
        _hook_data: &[u8],
    ) -> Result<BalanceDelta, Error> {
        Ok(BalanceDelta::DEFAULT)
    }

    fn before_remove_liquidity(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: &ModifyLiquidityParams,
        _hook_data: &[u8],
    ) -> Result<(), Error> {
        Ok(())
    }

    fn after_remove_liquidity(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: &ModifyLiquidityParams,
        _delta: BalanceDelta,
        _fees_accrued: BalanceDelta,
        _hook_data: &[u8],
    ) -> Result<BalanceDelta, Error> {
        Ok(BalanceDelta::DEFAULT)
    }

    fn before_swap(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        params: &SwapParams,
        env: &HookEnv<'_>,
        _hook_data: &[u8],
    ) -> Result<(BeforeSwapDelta, Fee), Error> {
        let fee = self.before_swap_fee(env, params.zero_for_one);
        Ok((
            BeforeSwapDelta::ZERO,
            Fee::new(fee).ok_or(Error::InvalidLpFee)?,
        ))
    }

    fn after_swap(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: &SwapParams,
        _delta: BalanceDelta,
        _hook_data: &[u8],
    ) -> Result<i128, Error> {
        Ok(0)
    }

    fn before_donate(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _amount0: U256,
        _amount1: U256,
        _hook_data: &[u8],
    ) -> Result<(), Error> {
        Ok(())
    }

    fn after_donate(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _amount0: U256,
        _amount1: U256,
        _hook_data: &[u8],
    ) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wad_math_matches_known_points() {
        let wad = I256::from_raw(U256::from(10u64.pow(18)));
        assert_eq!(ln_wad(wad), I256::ZERO);
        assert_eq!(exp_wad(I256::ZERO), wad);
        // Decay keeps shrinking across the k^n -> expWad switch at 5 blocks.
        let k = 11_744_051;
        let four = decaying_fee(0, ONE_E12, k, 4);
        let five = decaying_fee(0, ONE_E12, k, 5);
        assert!(
            five < four,
            "decay must keep shrinking across the fast/slow switch"
        );
    }
}
