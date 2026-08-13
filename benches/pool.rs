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
    let mut group = c.benchmark_group("pool_ticks");

    let pool = build_test_pool();
    let pool_ticks = pool.ticks;

    let tick_lower = pool_ticks.read().first_tick();
    let tick_upper = pool_ticks.read().last_tick();
    let current_tick = TickIndex::new(0).unwrap();

    group.bench_function("next_initialized_tick_zero_for_one", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let guard = pool.read();
            guard.next_initialized_tick(current_tick, true)
        })
    });

    group.bench_function("next_initialized_tick_one_for_zero", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let guard = pool.read();
            guard.next_initialized_tick(current_tick, false)
        })
    });

    group.bench_function("cross_tick", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let guard = pool.read();
            guard.cross_tick(tick_upper).unwrap()
        })
    });

    group.bench_function("update_tick", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let mut guard = pool.write();
            guard
                .update_tick(black_box(tick_upper), black_box(500_i128), black_box(false))
                .unwrap()
        })
    });

    group.bench_function("update_tick_pair", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let mut guard = pool.write();
            guard
                .update_tick_pair(
                    black_box(tick_lower),
                    black_box(tick_upper),
                    black_box(500_i128),
                    None,
                )
                .unwrap()
        })
    });

    group.finish();
}

fn bench_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool");

    let pool = build_test_pool();

    let tick_lower = pool.ticks.read().first_tick();
    let tick_upper = pool.ticks.read().last_tick();

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
