use alloy::primitives::{Address, B256, I256};
use rm_uniswap::{
    Error, Fee, Liquidity, ModifyLiquidityParams, Pool, PoolTicks, ProtocolFee, SqrtPriceX96,
    SwapFee, SwapParams, SwapParamsInner, TickIndex, TickSpacing,
    v4::{
        HookEnv, PoolKey,
        positions::SinglePoolManager,
        stable_pair::{StableFeeConfig, StableFeeState, StablePairHook},
    },
};
use ruint::aliases::U256;

fn config() -> StableFeeConfig {
    StableFeeConfig {
        k: 11_744_051,
        optimal_fee_e6: 7,
        target_multiplier: 100,
        reference_sqrt_price_x96: U256::ONE << 96,
    }
}

#[test]
fn new_rejects_configs_the_contract_rejects() {
    let state = StableFeeState::reset(0);
    assert!(StablePairHook::new(config(), state).is_ok());
    for bad in [
        StableFeeConfig { k: 0, ..config() },
        StableFeeConfig {
            k: 1 << 24,
            ..config()
        },
        StableFeeConfig {
            optimal_fee_e6: 10_001,
            ..config()
        },
        StableFeeConfig {
            target_multiplier: 101,
            ..config()
        },
        StableFeeConfig {
            reference_sqrt_price_x96: U256::ZERO,
            ..config()
        },
    ] {
        assert_eq!(
            StablePairHook::new(bad, state).unwrap_err(),
            Error::InvalidHookConfig
        );
    }
}

/// The manager must hand the hook its block number and pre-swap pool state,
/// and charge the hook's fee.
#[test]
fn manager_swaps_at_the_fee_the_hook_computes_from_its_env() {
    let spacing = TickSpacing::new(1).unwrap();
    // Price 1% below reference: outside the optimal range, so the fee is
    // direction- and block-dependent.
    let sqrt_price =
        SqrtPriceX96::from_u256((U256::ONE << 96) * U256::from(995) / U256::from(1000)).unwrap();
    let tick = sqrt_price.tick_index();
    let pool = Pool::try_new(
        sqrt_price,
        tick,
        Liquidity::ZERO,
        SwapFee::new_dynamic(ProtocolFee::ZERO),
        spacing,
        PoolTicks::new(spacing),
    )
    .unwrap();
    let key = PoolKey::new(
        Address::repeat_byte(0x01),
        Address::repeat_byte(0x02),
        None,
        1,
        Address::repeat_byte(0x03),
    );
    let hook = StablePairHook::new(config(), StableFeeState::reset(100)).unwrap();
    let mut expected_hook = hook.clone();

    let mut manager = SinglePoolManager::new(pool, Some((key, Box::new(hook))));
    manager
        .modify_liquidity(
            ModifyLiquidityParams {
                tick_lower: TickIndex::new(-500).unwrap(),
                tick_upper: TickIndex::new(500).unwrap(),
                liquidity_delta: 1_000_000_000_000_000,
                owner: Address::repeat_byte(0x04),
                salt: B256::ZERO,
            },
            b"",
        )
        .unwrap();
    manager.set_block_number(105);

    // Selling token1 moves price back toward reference, so a fee is charged.
    let amount = -I256::from(U256::from(1_000_000_000u64));
    let fee = expected_hook.before_swap_fee(
        &HookEnv {
            block_number: 105,
            pool_state: &manager.pool().state,
        },
        false,
    );
    assert!(fee > 0);
    let expected = manager
        .pool()
        .quote_swap(SwapParamsInner {
            lp_fee_override: Fee::new(fee),
            ..SwapParamsInner::new(false, amount)
        })
        .unwrap()
        .swap_delta;

    let delta = manager
        .swap(
            Address::repeat_byte(0x05),
            SwapParams::new(false, amount),
            b"",
        )
        .unwrap();
    assert_eq!(delta, expected);
}
