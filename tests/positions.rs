#![cfg(feature = "positions")]

use alloy::primitives::Address;
use rm_uniswap::v4::{Pool, PoolTicks, SignedAmount, SwapParams, positions::PositionManager};
use ruint::aliases::U256;
#[cfg(feature = "v4-hooks")]
use std::sync::Arc;

fn empty_pool() -> Pool {
    Pool::new(
        U256::ONE << 96u32,
        0,
        0,
        3_000,
        60,
        PoolTicks::new(60).unwrap(),
    )
}

#[test]
fn returns_fee_delta_separately_from_principal() {
    let owner = Address::repeat_byte(0x22);
    let manager = PositionManager::new(empty_pool());
    let minted = manager
        .mint(owner, -120, 120, 1_000_000_000_000, u128::MAX, u128::MAX)
        .unwrap();

    manager
        .pool()
        .swap(SwapParams::new(
            false,
            SignedAmount::negative(U256::from(1_000_000_000u64)),
        ))
        .unwrap();

    let collected = manager.collect_fees(owner, minted.token_id).unwrap();
    assert_eq!(collected.principal_delta.amount0, 0);
    assert_eq!(collected.principal_delta.amount1, 0);
    assert!(collected.fee_delta.amount0 > 0 || collected.fee_delta.amount1 > 0);

    manager
        .decrease_liquidity(owner, minted.token_id, collected.liquidity, 0, 0)
        .unwrap();
    manager.burn(owner, minted.token_id).unwrap();
}

#[test]
fn slippage_failure_does_not_mutate_pool() {
    let owner = Address::repeat_byte(0x33);
    let manager = PositionManager::new(empty_pool());
    let before = *manager.pool().state.read();

    assert!(manager.mint(owner, -120, 120, 1_000_000, 0, 0).is_err());
    let after = *manager.pool().state.read();
    assert_eq!(before.liquidity, after.liquidity);
    assert!(manager.pool().ticks.read().is_empty());
}

#[test]
fn fee_growth_outside_assigns_post_cross_fees_to_the_active_range() {
    let owner = Address::repeat_byte(0x44);
    let manager = PositionManager::new(empty_pool());
    let left = manager
        .mint(owner, -120, 0, 1_000_000_000_000, u128::MAX, u128::MAX)
        .unwrap();
    let right = manager
        .mint(owner, 0, 120, 1_000_000_000_000, u128::MAX, u128::MAX)
        .unwrap();

    manager
        .pool()
        .swap(SwapParams::new(
            true,
            SignedAmount::negative(U256::from(1_000_000_000u64)),
        ))
        .unwrap();

    let left_fees = manager.collect_fees(owner, left.token_id).unwrap();
    let right_fees = manager.collect_fees(owner, right.token_id).unwrap();
    assert!(left_fees.fee_delta.amount0 > right_fees.fee_delta.amount0);
}

#[cfg(feature = "v4-hooks")]
#[test]
fn v4_hook_failure_rolls_back_before_pool_mutation() {
    use rm_uniswap::{
        Error,
        v4::positions::{LiquidityHook, ModifyRequest, ModifyResult},
    };

    struct Reject;
    impl LiquidityHook for Reject {
        fn after_modify_liquidity(
            &self,
            _request: &ModifyRequest,
            _result: &ModifyResult,
        ) -> Result<rm_uniswap::v4::BalanceDelta, Error> {
            Err(Error::Unauthorized)
        }
    }

    let owner = Address::repeat_byte(0x55);
    let manager = PositionManager::with_hook(empty_pool(), Arc::new(Reject));
    assert!(
        manager
            .mint(owner, -120, 120, 1_000_000, u128::MAX, u128::MAX)
            .is_err()
    );
    assert_eq!(manager.pool().state.read().liquidity, 0);
    assert!(manager.pool().ticks.read().is_empty());
}
