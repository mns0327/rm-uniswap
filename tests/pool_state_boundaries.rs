use rm_uniswap::{
    Error,
    v4::{Pool, PoolTicks, SignedAmount, SwapParams, TickEntry, TickInfo, TickMath},
};
use ruint::aliases::U256;

const LIQUIDITY: u128 = 1_000_000_000_000;
const FEE: u32 = 3_000;
const TICK_SPACING: i32 = 60;

fn valid_pool() -> Pool {
    let ticks = PoolTicks::from_tick_entries(
        [
            TickEntry {
                tick_idx: -60,
                liquidity_net: LIQUIDITY as i128,
                liquidity_gross: LIQUIDITY,
            },
            TickEntry {
                tick_idx: 60,
                liquidity_net: -(LIQUIDITY as i128),
                liquidity_gross: LIQUIDITY,
            },
        ],
        TICK_SPACING,
    )
    .unwrap();

    Pool::try_new(U256::ONE << 96, 0, LIQUIDITY, FEE, TICK_SPACING, ticks).unwrap()
}

#[test]
fn snapshot_rejects_tick_spacing_mismatch() {
    let mut snapshot = valid_pool().snapshot();
    snapshot.ticks.tick_spacing = 10;

    assert_eq!(
        Pool::try_from(snapshot).unwrap_err(),
        Error::InvalidTickSpacing
    );
}

#[test]
fn snapshot_rejects_tick_and_price_mismatch() {
    let mut snapshot = valid_pool().snapshot();
    snapshot.state.tick = 1;

    assert_eq!(Pool::try_from(snapshot).unwrap_err(), Error::InvalidTick);
}

#[test]
fn snapshot_rejects_zero_gross_initialized_tick() {
    let mut snapshot = valid_pool().snapshot();
    snapshot.ticks.inner.insert(-120, TickInfo::default());

    assert_eq!(Pool::try_from(snapshot).unwrap_err(), Error::InvalidTick);
}

#[test]
fn snapshot_rejects_inconsistent_active_liquidity() {
    let mut snapshot = valid_pool().snapshot();
    snapshot.state.liquidity += 1;

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
fn shared_handle_and_fork_have_distinct_state_semantics() {
    let pool = valid_pool();
    let shared = pool.shared_handle();
    let fork = pool.try_fork().unwrap();

    pool.swap(SwapParams::new(
        false,
        SignedAmount::negative(U256::from(1_000_000u64)),
    ))
    .unwrap();

    assert_eq!(*pool.state.read(), *shared.state.read());
    assert_ne!(*pool.state.read(), *fork.state.read());
}

#[test]
fn mutating_swap_refreshes_scoring_price_cache() {
    let pool = valid_pool();
    let before = pool.spot_price_with_fee(false);

    let result = pool
        .swap(SwapParams::new(
            false,
            SignedAmount::negative(U256::from(1_000_000u64)),
        ))
        .unwrap();

    let after = pool.spot_price_with_fee(false);
    let expected_raw = {
        let sqrt = result.sqrt_price_x96.to::<u128>() as f64 / 2f64.powi(96);
        1.0 / (sqrt * sqrt)
    };
    let expected_with_fee = expected_raw * (1.0 - FEE as f64 / 1_000_000.0);

    assert!(after < before);
    assert!((after - expected_with_fee).abs() < 1e-12);
}

#[test]
fn sample_pool_snapshot_is_still_accepted() {
    let json = include_str!("../samples/pool1.json");
    let pool: Pool = serde_json::from_str(json).unwrap();

    let derived_tick = TickMath::get_tick_at_sqrt_price(pool.state.read().sqrt_price_x96).unwrap();
    assert_eq!(derived_tick, pool.state.read().tick);
}

#[test]
fn concurrent_quotes_and_swaps_keep_a_valid_shared_state() {
    let pool = valid_pool();
    let writer = pool.shared_handle();
    let writer_thread = std::thread::spawn(move || {
        for _ in 0..50 {
            writer
                .swap(SwapParams::new(
                    false,
                    SignedAmount::negative(U256::from(1_000u64)),
                ))
                .unwrap();
        }
    });

    let readers = (0..4)
        .map(|_| {
            let reader = pool.shared_handle();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    reader
                        .simulate_swap(SwapParams::new(
                            true,
                            SignedAmount::negative(U256::from(500u64)),
                        ))
                        .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();

    writer_thread.join().unwrap();
    for reader in readers {
        reader.join().unwrap();
    }

    pool.snapshot().validate().unwrap();
}

#[cfg(feature = "protocol-fee")]
#[test]
fn protocol_fee_is_collected_and_light_full_results_match() {
    let pool = valid_pool();
    let mut params = SwapParams::new(false, SignedAmount::negative(U256::from(100_000_000u64)));
    params.protocol_fee = Some(500);

    let light = pool.simulate_swap(params).unwrap();
    let full = pool.simulate_swap_full(params).unwrap();

    assert!(light.protocol_fee_amount > 0);
    assert_eq!(light.protocol_fee_amount, full.protocol_fee_amount);
    assert_eq!(light.delta, full.delta);
    assert_eq!(light.sqrt_price_x96, full.sqrt_price_x96);
}

#[cfg(feature = "protocol-fee")]
#[test]
fn protocol_fee_above_v4_maximum_is_rejected() {
    let pool = valid_pool();
    let mut params = SwapParams::new(true, SignedAmount::negative(U256::from(1_000_000u64)));
    params.protocol_fee = Some(1_001);

    assert_eq!(pool.simulate_swap(params).unwrap_err(), Error::FeeTooLarge);
}
