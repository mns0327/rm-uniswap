//! Benchmark suite for pool-level swap and liquidity operations.
//!
//! Run with: `cargo bench --bench pool`

use std::path::Path;

use alloy::primitives::{Address, B256, I256};
use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use rm_uniswap::{ModifyLiquidityParams, Pool, PoolTicks, SwapParams, TickIndex, TickSpacing};
use ruint::aliases::U256;

fn build_test_pool() -> Pool {
    let path = Path::new("samples/pool1.json");
    let file = std::fs::File::open(path).unwrap();
    serde_json::from_reader(file).unwrap()
}

fn ether(n: u128) -> U256 {
    U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
}

fn bench_pool_ticks(c: &mut Criterion) {
    let mut group = c.benchmark_group("tick_slab_pool_storage");

    let pool = build_test_pool();
    let tick_snapshot = pool.ticks.snapshot();
    let tick_upper = *tick_snapshot.keys().next_back().unwrap();
    let current_tick = TickIndex::new(0).unwrap();

    group.bench_function("slab_next_initialized_tick_zero_for_one", |b| {
        b.iter(|| {
            let indexer = pool.ticks.indexer(black_box(current_tick));
            black_box(
                pool.ticks
                    .next_initialized_tick(indexer, black_box(true))
                    .map(|(indexer, tick_info)| (indexer, tick_info.inner.liquidity_gross)),
            )
        })
    });

    group.bench_function("slab_next_initialized_tick_one_for_zero", |b| {
        b.iter(|| {
            let indexer = pool.ticks.indexer(black_box(current_tick));
            black_box(
                pool.ticks
                    .next_initialized_tick(indexer, black_box(false))
                    .map(|(indexer, tick_info)| (indexer, tick_info.inner.liquidity_gross)),
            )
        })
    });

    group.bench_function("cross_fee_growth", |b| {
        b.iter_batched(
            || PoolTicks::from_snapshot(pool.tick_spacing, tick_snapshot.clone()).unwrap(),
            |mut ticks| {
                let indexer = ticks.indexer(black_box(tick_upper));
                ticks
                    .cross_tick(
                        indexer,
                        black_box(U256::from(1_000_000u64)),
                        black_box(U256::from(2_000_000u64)),
                    )
                    .unwrap()
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("update_initialized_tick", |b| {
        b.iter_batched(
            || PoolTicks::from_snapshot(pool.tick_spacing, tick_snapshot.clone()).unwrap(),
            |mut ticks| {
                let indexer = ticks.indexer(black_box(tick_upper));
                ticks
                    .update_initialized_tick(indexer, |_, _, tick_info| {
                        tick_info.fee_growth_outside0_x128 = black_box(U256::from(500u64));
                        Ok(())
                    })
                    .unwrap()
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("snapshot", |b| b.iter(|| black_box(pool.ticks.snapshot())));

    group.finish();
}

fn bench_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool");

    let pool = build_test_pool();

    let tick_snapshot = pool.ticks.snapshot();
    let tick_lower = *tick_snapshot.keys().next().unwrap();
    let tick_upper = *tick_snapshot.keys().next_back().unwrap();

    group.bench_function("quote_swap_exact_in", |b| {
        b.iter(|| {
            pool.quote_swap(SwapParams::new(
                black_box(true),
                black_box(-I256::from(ether(10))),
            ))
            .unwrap()
        })
    });

    group.bench_function("quote_swap_exact_out", |b| {
        b.iter(|| {
            pool.quote_swap(SwapParams::new(
                black_box(true),
                black_box(I256::from(ether(1))),
            ))
            .unwrap()
        })
    });

    group.bench_function("swap_exact_in", |b| {
        b.iter_batched(
            build_test_pool,
            |mut pool| {
                pool.swap(SwapParams::new(
                    black_box(true),
                    black_box(-I256::from(ether(1))),
                ))
                .unwrap()
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("modify_liquidity", |b| {
        b.iter_batched(
            build_test_pool,
            |mut pool| {
                pool.modify_liquidity(ModifyLiquidityParams {
                    tick_lower: black_box(tick_lower),
                    tick_upper: black_box(tick_upper),
                    liquidity_delta: black_box(1_000_000_000_000_000_000i128),
                    owner: Address::ZERO,
                    salt: B256::ZERO,
                })
                .unwrap()
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn bench_utils(c: &mut Criterion) {
    let mut group = c.benchmark_group("utils");

    group.bench_function("tick_spacing_max_liquidity_per_tick", |b| {
        b.iter(|| {
            let tick_spacing = black_box(TickSpacing::new(60).unwrap());
            black_box(tick_spacing.max_liquidity_per_tick())
        })
    });

    group.finish();
}

criterion_group!(pool_benches, bench_pool_ticks, bench_pool, bench_utils);
criterion_main!(pool_benches);
