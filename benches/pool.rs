//! Benchmark suite for pool-level swap and liquidity operations.
//!
//! Run with: `cargo bench --bench pool`

use std::path::Path;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rm_uniswap::v4::{
    ModifyLiquidityParams, Pool, SignedAmount, SwapParams, TickIndex, TickSpacing,
};
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
    let pool_ticks = pool.ticks.clone();

    let tick_snapshot = pool_ticks.read().snapshot();
    let tick_upper = *tick_snapshot.keys().next_back().unwrap();
    let current_tick = TickIndex::new(0).unwrap();

    group.bench_function("slab_next_initialized_tick_zero_for_one", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let guard = pool.read();
            let indexer = guard.indexer(black_box(current_tick));
            black_box(
                guard
                    .next_initialized_tick(indexer, black_box(true))
                    .map(|(indexer, tick_info)| (indexer, tick_info.liquidity_gross)),
            )
        })
    });

    group.bench_function("slab_next_initialized_tick_one_for_zero", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let guard = pool.read();
            let indexer = guard.indexer(black_box(current_tick));
            black_box(
                guard
                    .next_initialized_tick(indexer, black_box(false))
                    .map(|(indexer, tick_info)| (indexer, tick_info.liquidity_gross)),
            )
        })
    });

    group.bench_function("cross_fee_growth", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let mut guard = pool.write();
            guard
                .cross_fee_growth(
                    black_box(tick_upper),
                    black_box(U256::from(1_000_000u64)),
                    black_box(U256::from(2_000_000u64)),
                )
                .unwrap()
        })
    });

    group.bench_function("update_initialized_tick", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let mut guard = pool.write();
            let indexer = guard.indexer(black_box(tick_upper));
            guard.update_initialized_tick(indexer, |tick_info| {
                tick_info.fee_growth_outside0_x128 = black_box(U256::from(500u64));
                black_box(true)
            })
        })
    });

    group.bench_function("snapshot", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let guard = pool.read();
            black_box(guard.snapshot())
        })
    });

    group.finish();
}

fn bench_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool");

    let pool = build_test_pool();

    let tick_snapshot = pool.ticks.read().snapshot();
    let tick_lower = *tick_snapshot.keys().next().unwrap();
    let tick_upper = *tick_snapshot.keys().next_back().unwrap();

    group.bench_function("simulate_swap_exact_in", |b| {
        b.iter(|| {
            pool.simulate_swap(SwapParams::new(
                black_box(true),
                black_box(SignedAmount::negative(ether(10))),
            ))
            .unwrap()
        })
    });

    group.bench_function("simulate_swap_exact_out", |b| {
        b.iter(|| {
            pool.simulate_swap(SwapParams::new(
                black_box(true),
                black_box(SignedAmount::positive(ether(1))),
            ))
            .unwrap()
        })
    });

    group.bench_function("quote_exact_input", |b| {
        b.iter(|| {
            pool.quote_exact_input(black_box(ether(1)), black_box(true))
                .unwrap()
        })
    });

    group.bench_function("swap_exact_in", |b| {
        b.iter(|| {
            pool.swap(SwapParams::new(
                black_box(true),
                black_box(SignedAmount::negative(ether(1))),
            ))
        })
    });

    group.bench_function("modify_liquidity", |b| {
        b.iter(|| {
            pool.modify_liquidity(ModifyLiquidityParams {
                tick_lower,
                tick_upper,
                liquidity_delta: 1_000_000_000_000_000_000i128,
            })
            .unwrap()
        })
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
