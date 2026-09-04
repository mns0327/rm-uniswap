//! Benchmark suite for tick math.
//!
//! Run with: `cargo bench --bench tick`

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rm_uniswap::{SqrtPriceX96, TickIndex, tick_math};

fn tick(value: i32) -> TickIndex {
    TickIndex::new(value).unwrap()
}

fn bench_tick_math(c: &mut Criterion) {
    let mut group = c.benchmark_group("tick_math");

    group.bench_function("tick_math::get_sqrt_price_at_tick", |b| {
        b.iter(|| black_box(tick_math::get_sqrt_price_at_tick(tick(60))))
    });

    group.bench_function("get_sqrt_price_at_tick_min", |b| {
        b.iter(|| black_box(tick_math::get_sqrt_price_at_tick(TickIndex::MIN)))
    });

    group.bench_function("get_sqrt_price_at_tick_max", |b| {
        b.iter(|| black_box(tick_math::get_sqrt_price_at_tick(TickIndex::MAX)))
    });

    let sqrt_p = tick_math::get_sqrt_price_at_tick(tick(60));
    group.bench_function("tick_math::get_tick_at_sqrt_price", |b| {
        b.iter(|| black_box(tick_math::get_tick_at_sqrt_price(&sqrt_p)))
    });

    group.bench_function("get_tick_at_sqrt_price_min", |b| {
        b.iter(|| black_box(tick_math::get_tick_at_sqrt_price(&SqrtPriceX96::MIN)))
    });

    group.bench_function("get_tick_at_sqrt_price_max", |b| {
        b.iter(|| black_box(tick_math::get_tick_at_sqrt_price(&SqrtPriceX96::MAX)))
    });

    group.finish();
}

criterion_group!(tick_benches, bench_tick_math);
criterion_main!(tick_benches);
