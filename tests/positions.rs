#![cfg(feature = "positions")]

use alloy::primitives::{Address, I256};
use rm_uniswap::v4::{
    Fee, Liquidity, Pool, PoolTicks, SqrtPriceX96, SwapParams, TickIndex, TickSpacing,
    positions::PositionManager,
};
use ruint::aliases::U256;
#[cfg(feature = "v4-hooks")]
use std::sync::Arc;

fn tick(index: i32) -> TickIndex {
    TickIndex::new(index).unwrap()
}

fn sqrt_price_1_1() -> SqrtPriceX96 {
    SqrtPriceX96::from_u256(U256::ONE << 96u32).unwrap()
}

fn tick_spacing_60() -> TickSpacing {
    TickSpacing::new(60).unwrap()
}

fn empty_pool() -> Pool {
    Pool::try_new(
        sqrt_price_1_1(),
        tick(0),
        Liquidity::ZERO,
        Fee::new(3_000).unwrap(),
        tick_spacing_60(),
        PoolTicks::new(tick_spacing_60()).unwrap(),
    )
    .unwrap()
}

#[test]
fn returns_fee_delta_separately_from_principal() {
    let owner = Address::repeat_byte(0x22);
    let manager = PositionManager::new(empty_pool());
    let minted = manager
        .mint(
            owner,
            tick(-120),
            tick(120),
            1_000_000_000_000,
            u128::MAX,
            u128::MAX,
        )
        .unwrap();

    manager
        .pool()
        .swap(SwapParams::new(
            false,
            -I256::from(U256::from(1_000_000_000u64)),
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
fn newly_minted_position_does_not_collect_prior_fees() {
    let early_owner = Address::repeat_byte(0x23);
    let late_owner = Address::repeat_byte(0x24);
    let manager = PositionManager::new(empty_pool());

    manager
        .mint(
            early_owner,
            tick(-120),
            tick(120),
            1_000_000_000_000,
            u128::MAX,
            u128::MAX,
        )
        .unwrap();

    manager
        .pool()
        .swap(SwapParams::new(
            false,
            -I256::from(U256::from(1_000_000_000u64)),
        ))
        .unwrap();

    let late_position = manager
        .mint(
            late_owner,
            tick(-120),
            tick(120),
            1_000_000_000_000,
            u128::MAX,
            u128::MAX,
        )
        .unwrap();

    let collected = manager
        .collect_fees(late_owner, late_position.token_id)
        .unwrap();
    assert_eq!(collected.fee_delta.amount0, 0);
    assert_eq!(collected.fee_delta.amount1, 0);
}

#[test]
fn slippage_failure_does_not_mutate_pool() {
    let owner = Address::repeat_byte(0x33);
    let manager = PositionManager::new(empty_pool());
    let before = *manager.pool().state.read();

    assert!(
        manager
            .mint(owner, tick(-120), tick(120), 1_000_000, 0, 0)
            .is_err()
    );
    let after = *manager.pool().state.read();
    assert_eq!(before.liquidity, after.liquidity);
    assert!(manager.pool().ticks.is_empty());
}

#[test]
fn fee_growth_outside_assigns_post_cross_fees_to_the_active_range() {
    let owner = Address::repeat_byte(0x44);
    let manager = PositionManager::new(empty_pool());
    let left = manager
        .mint(
            owner,
            tick(-120),
            tick(0),
            1_000_000_000_000,
            u128::MAX,
            u128::MAX,
        )
        .unwrap();
    let right = manager
        .mint(
            owner,
            tick(0),
            tick(120),
            1_000_000_000_000,
            u128::MAX,
            u128::MAX,
        )
        .unwrap();

    manager
        .pool()
        .swap(SwapParams::new(
            true,
            -I256::from(U256::from(1_000_000_000u64)),
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
            .mint(
                owner,
                tick(-120),
                tick(120),
                1_000_000,
                u128::MAX,
                u128::MAX
            )
            .is_err()
    );
    assert_eq!(manager.pool().state.read().liquidity, Liquidity::ZERO);
    assert!(manager.pool().ticks.is_empty());
}

#[cfg(feature = "v4-hooks")]
#[test]
fn before_hook_failure_rolls_back_before_pool_mutation() {
    use rm_uniswap::{
        Error,
        v4::positions::{LiquidityHook, ModifyRequest},
    };

    struct Reject;
    impl LiquidityHook for Reject {
        fn before_modify_liquidity(&self, _request: &ModifyRequest) -> Result<(), Error> {
            Err(Error::Unauthorized)
        }
    }

    let owner = Address::repeat_byte(0x56);
    let manager = PositionManager::with_hook(empty_pool(), Arc::new(Reject));
    assert_eq!(
        manager
            .mint(
                owner,
                tick(-120),
                tick(120),
                1_000_000,
                u128::MAX,
                u128::MAX
            )
            .unwrap_err(),
        Error::Unauthorized
    );
    assert_eq!(manager.pool().state.read().liquidity, Liquidity::ZERO);
    assert!(manager.pool().ticks.is_empty());
}

#[cfg(feature = "v4-hooks")]
#[test]
fn hooks_run_in_before_then_after_order() {
    use parking_lot::Mutex;
    use rm_uniswap::v4::{
        BalanceDelta,
        positions::{LiquidityHook, ModifyRequest, ModifyResult},
    };

    struct Recorder(Arc<Mutex<Vec<&'static str>>>);
    impl LiquidityHook for Recorder {
        fn before_modify_liquidity(
            &self,
            _request: &ModifyRequest,
        ) -> Result<(), rm_uniswap::Error> {
            self.0.lock().push("before");
            Ok(())
        }

        fn after_modify_liquidity(
            &self,
            _request: &ModifyRequest,
            result: &ModifyResult,
        ) -> Result<BalanceDelta, rm_uniswap::Error> {
            assert_eq!(result.liquidity, 1_000_000);
            self.0.lock().push("after");
            Ok(BalanceDelta::DEFAULT)
        }
    }

    let events = Arc::new(Mutex::new(Vec::new()));
    let owner = Address::repeat_byte(0x57);
    let manager = PositionManager::with_hook(empty_pool(), Arc::new(Recorder(Arc::clone(&events))));
    manager
        .mint(
            owner,
            tick(-120),
            tick(120),
            1_000_000,
            u128::MAX,
            u128::MAX,
        )
        .unwrap();

    assert_eq!(&*events.lock(), &["before", "after"]);
}

#[cfg(feature = "v4-hooks")]
#[test]
fn hook_runs_without_holding_position_map_lock() {
    use parking_lot::Mutex;
    use rm_uniswap::v4::positions::{LiquidityHook, ModifyRequest};
    use std::{
        sync::mpsc::{self, Receiver, Sender},
        time::Duration,
    };

    struct BlockingHook {
        entered: Sender<()>,
        release: Mutex<Receiver<()>>,
    }
    impl LiquidityHook for BlockingHook {
        fn before_modify_liquidity(
            &self,
            _request: &ModifyRequest,
        ) -> Result<(), rm_uniswap::Error> {
            self.entered.send(()).unwrap();
            self.release.lock().recv().unwrap();
            Ok(())
        }
    }

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let owner = Address::repeat_byte(0x58);
    let manager = Arc::new(PositionManager::with_hook(
        empty_pool(),
        Arc::new(BlockingHook {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }),
    ));
    let mint_manager = Arc::clone(&manager);
    let mint_thread = std::thread::spawn(move || {
        mint_manager.mint(
            owner,
            tick(-120),
            tick(120),
            1_000_000,
            u128::MAX,
            u128::MAX,
        )
    });

    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let inspect_manager = Arc::clone(&manager);
    let (inspected_tx, inspected_rx) = mpsc::channel();
    let inspect_thread = std::thread::spawn(move || {
        inspected_tx.send(inspect_manager.position(1)).unwrap();
    });
    let inspected = inspected_rx.recv_timeout(Duration::from_millis(250));

    release_tx.send(()).unwrap();
    inspect_thread.join().unwrap();
    mint_thread.join().unwrap().unwrap();

    assert!(inspected.unwrap().is_some());
}

#[cfg(feature = "v4-hooks")]
#[test]
fn hook_delta_overflow_fails_before_pool_or_position_mutation() {
    use rm_uniswap::{
        Error,
        v4::{
            BalanceDelta,
            positions::{LiquidityHook, ModifyRequest, ModifyResult},
        },
    };

    struct OverflowOnCollect;
    impl LiquidityHook for OverflowOnCollect {
        fn after_modify_liquidity(
            &self,
            request: &ModifyRequest,
            _result: &ModifyResult,
        ) -> Result<BalanceDelta, Error> {
            Ok(if request.liquidity_delta == 0 {
                BalanceDelta {
                    amount0: i128::MAX,
                    amount1: i128::MAX,
                }
            } else {
                BalanceDelta::DEFAULT
            })
        }
    }

    let owner = Address::repeat_byte(0x59);
    let manager = PositionManager::with_hook(empty_pool(), Arc::new(OverflowOnCollect));
    let minted = manager
        .mint(
            owner,
            tick(-120),
            tick(120),
            1_000_000_000_000,
            u128::MAX,
            u128::MAX,
        )
        .unwrap();
    manager
        .pool()
        .swap(SwapParams::new(
            false,
            -I256::from(U256::from(1_000_000_000u64)),
        ))
        .unwrap();

    let pool_before = manager.pool().snapshot();
    let position_before = manager.position(minted.token_id).unwrap();

    assert_eq!(
        manager.collect_fees(owner, minted.token_id).unwrap_err(),
        Error::AmountOverflow
    );
    assert_eq!(manager.pool().snapshot(), pool_before);
    assert_eq!(manager.position(minted.token_id).unwrap(), position_before);
}
