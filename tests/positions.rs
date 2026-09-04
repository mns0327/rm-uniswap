use alloy::primitives::{Address, B256, I256};
use parking_lot::Mutex;
use rm_uniswap::v4::{
    BalanceDelta, BeforeSwapDelta, Error, Fee, Hooks, HooksImpl, Liquidity, ModifyLiquidityParams,
    Pool, PoolKey, PoolTicks, SqrtPriceX96, SwapParams, TickIndex, TickSpacing,
    positions::{PositionIndex, SinglePoolManager},
};
use ruint::aliases::U256;
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
        PoolTicks::new(tick_spacing_60()),
    )
    .unwrap()
}

fn pool_key() -> PoolKey {
    PoolKey::new(
        Address::repeat_byte(0x01),
        Address::repeat_byte(0x02),
        3_000,
        60,
        Address::repeat_byte(0x03),
    )
}

fn salt(byte: u8) -> B256 {
    B256::repeat_byte(byte)
}

fn position_params(
    owner: Address,
    salt: B256,
    tick_lower: TickIndex,
    tick_upper: TickIndex,
    liquidity_delta: i128,
) -> ModifyLiquidityParams {
    ModifyLiquidityParams {
        tick_lower,
        tick_upper,
        liquidity_delta,
        owner,
        salt,
    }
}

#[test]
fn liquidity_modification_realizes_fees_through_positions() {
    let owner = Address::repeat_byte(0x22);
    let mut manager = SinglePoolManager::new(empty_pool(), None);
    let add = position_params(owner, salt(0x11), tick(-120), tick(120), 1_000_000_000_000);

    manager.modify_liquidity(add, b"").unwrap();
    manager
        .pool_mut()
        .swap(SwapParams::new(
            false,
            -I256::from(U256::from(1_000_000_000u64)),
        ))
        .unwrap();

    let collect = ModifyLiquidityParams {
        liquidity_delta: 0,
        ..add
    };
    let collected = manager.modify_liquidity(collect, b"").unwrap();

    assert_eq!(collected.delta, collected.fee_delta);
    assert!(collected.fee_delta.amount0 > 0 || collected.fee_delta.amount1 > 0);
}

#[test]
fn sender_overrides_spoofed_position_owner() {
    let sender = Address::repeat_byte(0x33);
    let params = position_params(sender, salt(0x22), tick(-120), tick(120), 1_000_000);
    let mut manager = SinglePoolManager::new(empty_pool(), None);

    manager.modify_liquidity(params, b"").unwrap();

    let actual = PositionIndex {
        owner: sender,
        tick_lower: params.tick_lower,
        tick_upper: params.tick_upper,
        salt: params.salt,
    };

    assert!(manager.pool().positions.0.contains_key(&actual));
}

struct ScriptedHook {
    events: Arc<Mutex<Vec<&'static str>>>,
    before_add_error: Option<Error>,
    after_add_error: Option<Error>,
    after_modify_delta: BalanceDelta,
    before_swap_delta: BeforeSwapDelta,
    after_swap_delta: i128,
}

impl Default for ScriptedHook {
    fn default() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            before_add_error: None,
            after_add_error: None,
            after_modify_delta: BalanceDelta::DEFAULT,
            before_swap_delta: BeforeSwapDelta::ZERO,
            after_swap_delta: 0,
        }
    }
}

impl HooksImpl for ScriptedHook {}

impl Hooks for ScriptedHook {
    fn before_add_liquidity(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: ModifyLiquidityParams,
        _hook_data: &[u8],
    ) -> Result<(), Error> {
        self.events.lock().push("before_add_liquidity");
        if let Some(error) = self.before_add_error {
            return Err(error);
        }
        Ok(())
    }

    fn after_add_liquidity(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: ModifyLiquidityParams,
        _delta: BalanceDelta,
        _fees_accrued: BalanceDelta,
        _hook_data: &[u8],
    ) -> Result<BalanceDelta, Error> {
        self.events.lock().push("after_add_liquidity");
        if let Some(error) = self.after_add_error {
            return Err(error);
        }
        Ok(self.after_modify_delta)
    }

    fn before_remove_liquidity(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: ModifyLiquidityParams,
        _hook_data: &[u8],
    ) -> Result<(), Error> {
        self.events.lock().push("before_remove_liquidity");
        Ok(())
    }

    fn after_remove_liquidity(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: ModifyLiquidityParams,
        _delta: BalanceDelta,
        _fees_accrued: BalanceDelta,
        _hook_data: &[u8],
    ) -> Result<BalanceDelta, Error> {
        self.events.lock().push("after_remove_liquidity");
        Ok(self.after_modify_delta)
    }

    fn before_swap(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: SwapParams,
        _hook_data: &[u8],
    ) -> Result<(BeforeSwapDelta, Fee), Error> {
        self.events.lock().push("before_swap");
        Ok((
            BeforeSwapDelta {
                specified_delta: self.before_swap_delta.specified_delta,
                unspecified_delta: self.before_swap_delta.unspecified_delta,
            },
            Fee::ZERO,
        ))
    }

    fn after_swap(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _params: SwapParams,
        _delta: BalanceDelta,
        _hook_data: &[u8],
    ) -> Result<i128, Error> {
        self.events.lock().push("after_swap");
        Ok(self.after_swap_delta)
    }

    fn before_donate(
        &mut self,
        _sender: &Address,
        _pool_key: &PoolKey,
        _amount0: U256,
        _amount1: U256,
        _hook_data: &[u8],
    ) -> Result<(), Error> {
        self.events.lock().push("before_donate");
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
        self.events.lock().push("after_donate");
        Ok(())
    }
}

#[test]
fn before_hook_failure_leaves_pool_unchanged() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let hook = ScriptedHook {
        events: Arc::clone(&events),
        before_add_error: Some(Error::Unauthorized),
        ..ScriptedHook::default()
    };
    let owner = Address::repeat_byte(0x55);
    let mut manager = SinglePoolManager::new(empty_pool(), Some((pool_key(), Box::new(hook))));
    let before = manager.pool().snapshot();

    assert_eq!(
        manager
            .modify_liquidity(
                position_params(owner, salt(0x33), tick(-120), tick(120), 1_000_000),
                b"reject",
            )
            .unwrap_err(),
        Error::Unauthorized
    );
    assert_eq!(manager.pool().snapshot(), before);
    assert_eq!(&*events.lock(), &["before_add_liquidity"]);
}

#[test]
fn after_hook_failure_propagates_after_pool_mutation() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let hook = ScriptedHook {
        events: Arc::clone(&events),
        after_add_error: Some(Error::Unauthorized),
        ..ScriptedHook::default()
    };
    let owner = Address::repeat_byte(0x56);
    let mut manager = SinglePoolManager::new(empty_pool(), Some((pool_key(), Box::new(hook))));
    let before = manager.pool().snapshot();

    assert_eq!(
        manager
            .modify_liquidity(
                position_params(owner, salt(0x44), tick(-120), tick(120), 1_000_000),
                b"reject-after",
            )
            .unwrap_err(),
        Error::Unauthorized
    );
    assert_ne!(manager.pool().snapshot(), before);
    assert_eq!(manager.pool().state.liquidity, Liquidity::new(1_000_000));
    assert_eq!(
        &*events.lock(),
        &["before_add_liquidity", "after_add_liquidity"]
    );
}

#[test]
fn swap_hooks_run_in_order_and_adjust_caller_delta() {
    let key = pool_key();
    let owner = Address::repeat_byte(0x57);
    let trader = Address::repeat_byte(0x58);
    let add = position_params(owner, salt(0x55), tick(-120), tick(120), 1_000_000_000_000);
    let swap = SwapParams::new(false, -I256::from(U256::from(1_000_000_000u64)));

    let mut baseline = SinglePoolManager::new(empty_pool(), None);
    baseline.modify_liquidity(add, b"").unwrap();
    let raw_delta = baseline.swap(trader, swap, b"").unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let hook = ScriptedHook {
        events: Arc::clone(&events),
        before_swap_delta: BeforeSwapDelta {
            specified_delta: 0,
            unspecified_delta: 7,
        },
        after_swap_delta: 5,
        ..ScriptedHook::default()
    };
    let mut manager = SinglePoolManager::new(empty_pool(), Some((key, Box::new(hook))));
    manager.modify_liquidity(add, b"").unwrap();
    events.lock().clear();

    let caller_delta = manager.swap(trader, swap, b"swap").unwrap();
    let expected = raw_delta
        .checked_sub(&BalanceDelta {
            amount0: 12,
            amount1: 0,
        })
        .unwrap();

    assert_eq!(caller_delta, expected);
    assert_eq!(&*events.lock(), &["before_swap", "after_swap"]);
}

#[test]
fn zero_swap_is_rejected() {
    let mut manager = SinglePoolManager::new(empty_pool(), None);

    assert_eq!(
        manager
            .swap(
                Address::repeat_byte(0x59),
                SwapParams::new(true, I256::ZERO),
                b"",
            )
            .unwrap_err(),
        Error::ZeroValue
    );
}

#[test]
fn donate_runs_hooks_without_mutating_pool() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let hook = ScriptedHook {
        events: Arc::clone(&events),
        ..ScriptedHook::default()
    };
    let mut manager = SinglePoolManager::new(empty_pool(), Some((pool_key(), Box::new(hook))));
    let before = manager.pool().snapshot();

    let delta = manager
        .donate(
            Address::repeat_byte(0x60),
            U256::from(100u64),
            U256::from(200u64),
            b"donate",
        )
        .unwrap();

    assert_eq!(delta, BalanceDelta::DEFAULT);
    assert_eq!(manager.pool().snapshot(), before);
    assert_eq!(&*events.lock(), &["before_donate", "after_donate"]);
}
