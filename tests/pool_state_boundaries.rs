use std::collections::BTreeMap;

use alloy::primitives::I256;
use rm_uniswap::{
    Error,
    v4::{
        Fee, Liquidity, Pool, PoolTicks, ProtocolFee, SqrtPriceX96, SwapParams, TickIndex,
        TickInfo, TickSpacing, tick_math,
    },
};
use ruint::aliases::U256;

const LIQUIDITY: u128 = 1_000_000_000_000;
const FEE: Fee = Fee::new(3_000).unwrap();

fn tick_spacing_60() -> TickSpacing {
    TickSpacing::new(60).unwrap()
}

fn tick(index: i32) -> TickIndex {
    TickIndex::new(index).unwrap()
}

fn sqrt_price_1_1() -> SqrtPriceX96 {
    SqrtPriceX96::from_u256(U256::ONE << 96).unwrap()
}

fn exact_input(raw: u64) -> I256 {
    -I256::from(U256::from(raw))
}

fn valid_pool() -> Pool {
    let ticks = PoolTicks::from_snapshot(
        tick_spacing_60(),
        BTreeMap::from([
            (
                tick(-60),
                TickInfo {
                    liquidity_net: LIQUIDITY as i128,
                    liquidity_gross: Liquidity::new(LIQUIDITY),
                    ..TickInfo::default()
                },
            ),
            (
                tick(60),
                TickInfo {
                    liquidity_net: -(LIQUIDITY as i128),
                    liquidity_gross: Liquidity::new(LIQUIDITY),
                    ..TickInfo::default()
                },
            ),
        ]),
    )
    .unwrap();

    Pool::try_new(
        sqrt_price_1_1(),
        tick(0),
        Liquidity::new(LIQUIDITY),
        FEE,
        tick_spacing_60(),
        ticks,
    )
    .unwrap()
}

#[test]
fn snapshot_rejects_tick_spacing_mismatch() {
    let mut snapshot = valid_pool().snapshot();
    snapshot.ticks.tick_spacing = TickSpacing::new(10).unwrap();

    assert_eq!(
        Pool::try_from(snapshot).unwrap_err(),
        Error::InvalidTickSpacing
    );
}

#[test]
fn snapshot_rejects_tick_and_price_mismatch() {
    let mut snapshot = valid_pool().snapshot();
    snapshot.state.tick = tick(1);

    assert_eq!(Pool::try_from(snapshot).unwrap_err(), Error::InvalidTick);
}

#[test]
fn snapshot_rejects_zero_gross_initialized_tick() {
    let mut snapshot = valid_pool().snapshot();
    snapshot
        .ticks
        .inner
        .insert(tick(-120), TickInfo::default().into());

    assert_eq!(Pool::try_from(snapshot).unwrap_err(), Error::InvalidTick);
}

#[test]
fn snapshot_rejects_inconsistent_active_liquidity() {
    let mut snapshot = valid_pool().snapshot();
    snapshot.state.liquidity = Liquidity::new(snapshot.state.liquidity.value() + 1);

    assert_eq!(Pool::try_from(snapshot).unwrap_err(), Error::InvalidTick);
}

#[test]
fn serde_rejects_untrusted_invalid_pool_snapshot() {
    let pool = valid_pool();
    let mut value = serde_json::to_value(pool.snapshot()).unwrap();
    value["tick_spacing"] = serde_json::json!(0);

    let decoded = serde_json::from_value::<Pool>(value);
    assert!(decoded.is_err());
}

#[test]
fn quote_swap_matches_committed_swap_without_mutating_source_pool() {
    let pool = valid_pool();
    let before = pool.snapshot();
    let params = SwapParams::new(false, exact_input(1_000_000));

    let quote = pool.quote_swap(params).unwrap();
    assert_eq!(pool.snapshot(), before, "quote must be read-only");

    let mut fork = pool.try_fork().unwrap();
    let committed = fork.swap(params).unwrap();

    assert_eq!(quote, committed);
    assert_eq!(
        pool.snapshot(),
        before,
        "committed fork must not touch source"
    );
    assert_ne!(
        fork.snapshot(),
        before,
        "committed swap must update its pool"
    );
}

#[test]
fn snapshot_and_fork_preserve_protocol_fee() {
    let mut pool = valid_pool();
    let protocol_fee = ProtocolFee::new(123, 456).unwrap();
    pool.set_protocol_fee(protocol_fee).unwrap();
    let params = SwapParams::new(false, exact_input(1_000_000));

    let snapshot = pool.snapshot();
    assert_eq!(snapshot.protocol_fee, protocol_fee);

    let fork = pool.try_fork().unwrap();
    assert_eq!(*fork.swap_fee.protocol_fee(), protocol_fee);
    assert_eq!(
        fork.quote_swap(params).unwrap(),
        pool.quote_swap(params).unwrap()
    );
}

#[test]
fn mutating_swap_commits_result_state_and_fee_growth() {
    let mut pool = valid_pool();
    let before = pool.snapshot();

    let result = pool
        .swap(SwapParams::new(false, exact_input(1_000_000)))
        .unwrap();

    let after = pool.snapshot();
    assert_ne!(after.state.sqrt_price_x96, before.state.sqrt_price_x96);
    assert_eq!(after.state.sqrt_price_x96, result.sqrt_price_x96);
    assert_eq!(after.state.tick, result.tick);
    assert_eq!(after.state.liquidity, result.liquidity);
    assert_eq!(result.swap_fee, FEE);
    assert_eq!(result.amount_to_protocol, U256::ZERO);
    assert_eq!(after.state.fee_growth_global0_x128, U256::ZERO);
    assert!(after.state.fee_growth_global1_x128 > before.state.fee_growth_global1_x128);
}

#[test]
fn quote_swap_uses_protocol_fee_in_effective_swap_fee() {
    let mut pool = valid_pool();
    let params = SwapParams::new(false, exact_input(1_000_000));

    let without_protocol_fee = pool.quote_swap(params).unwrap();

    pool.set_protocol_fee(ProtocolFee::new(0, 500).unwrap())
        .unwrap();
    let with_protocol_fee = pool.quote_swap(params).unwrap();

    let effective_fee = 500 + FEE.pips() - (500 * FEE.pips() / 1_000_000);

    assert_eq!(without_protocol_fee.swap_fee, FEE);
    assert_eq!(without_protocol_fee.amount_to_protocol, U256::ZERO);
    assert_eq!(with_protocol_fee.swap_fee, Fee::new(effective_fee).unwrap());
    assert!(with_protocol_fee.amount_to_protocol > U256::ZERO);
    assert!(
        with_protocol_fee.swap_delta.amount_out(false)
            < without_protocol_fee.swap_delta.amount_out(false)
    );
}

#[test]
fn crossing_quote_is_read_only_but_committed_swap_updates_fee_growth_outside() {
    let pool = valid_pool();
    let params = SwapParams {
        zero_for_one: true,
        amount: -I256::from(U256::from(1_000_000_000_000_000u64)),
        sqrt_price_limit_x96: tick(-60).sqrt_price_x96(),
    };

    let quote = pool.quote_swap(params).unwrap();
    let quoted_snapshot = pool.snapshot();
    let quoted_lower = quoted_snapshot.ticks.inner.get(&tick(-60)).unwrap();
    assert_eq!(quoted_lower.fee_growth_outside0_x128, U256::ZERO);

    let mut committed_pool = pool.try_fork().unwrap();
    let committed = committed_pool.swap(params).unwrap();
    let committed_snapshot = committed_pool.snapshot();
    let committed_lower = committed_snapshot.ticks.inner.get(&tick(-60)).unwrap();

    assert_eq!(quote, committed);
    assert_eq!(committed.tick, tick(-61));
    assert_eq!(committed.liquidity, Liquidity::ZERO);
    assert!(committed.swap_delta.amount_in(true) > 0);
    assert!(committed.swap_delta.amount_out(true) > 0);
    assert!(committed_snapshot.state.fee_growth_global0_x128 > U256::ZERO);
    assert_eq!(
        committed_lower.fee_growth_outside0_x128,
        committed_snapshot.state.fee_growth_global0_x128
    );
}

#[test]
fn sample_pool_snapshot_is_still_accepted() {
    let json = include_str!("../samples/pool1.json");
    let pool: Pool = serde_json::from_str(json).unwrap();

    let derived_tick = tick_math::get_tick_at_sqrt_price(&pool.state.read().sqrt_price_x96);
    assert_eq!(derived_tick, pool.state.read().tick);
    assert_eq!(*pool.swap_fee.protocol_fee(), ProtocolFee::ZERO);
}

#[test]
fn serde_rejects_invalid_protocol_fee_snapshot() {
    let mut value = serde_json::to_value(valid_pool().snapshot()).unwrap();
    value["protocol_fee"] = serde_json::json!({
        "zero_for_one_fee": 1_001,
        "one_for_zero_fee": 0
    });

    let decoded = serde_json::from_value::<Pool>(value);
    assert!(decoded.is_err());
}
