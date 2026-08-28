//! Benchmark suite for swap-step math.
//!
//! Run with: `cargo bench --bench swap`

use alloy::primitives::I256;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rm_uniswap::v4::{Fee, NonZeroLiquidity, SqrtPriceX96, TickIndex, swap_math, tick_math};
use ruint::aliases::U256;

fn tick(value: i32) -> TickIndex {
    TickIndex::new(value).unwrap()
}

fn sqrt_price_1_1() -> SqrtPriceX96 {
    SqrtPriceX96::from_u256(U256::ONE << 96).unwrap()
}

fn ether(n: u128) -> U256 {
    U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
}

fn bench_swap_math(c: &mut Criterion) {
    let mut group = c.benchmark_group("swap_math");

    let sqrt_current = sqrt_price_1_1();
    let sqrt_target = tick_math::get_sqrt_price_at_tick(tick(60));
    let liquidity = NonZeroLiquidity::new(1_000_000_000_000_000_000u128).unwrap();
    let fee = Fee::new(3000u32).unwrap();

    group.bench_function("compute_swap_step_exact_in", |b| {
        b.iter(|| {
            swap_math::compute_swap_step(
                black_box(sqrt_current),
                black_box(sqrt_target),
                black_box(liquidity),
                black_box(-I256::from(ether(1))),
                black_box(fee),
            )
        })
    });

    group.bench_function("compute_swap_step_exact_out", |b| {
        b.iter(|| {
            swap_math::compute_swap_step(
                black_box(sqrt_current),
                black_box(sqrt_target),
                black_box(liquidity),
                black_box(I256::from(ether(1))),
                black_box(fee),
            )
        })
    });

    group.bench_function("swap_math::get_sqrt_price_target", |b| {
        b.iter(|| {
            swap_math::get_sqrt_price_target(
                black_box(true),
                black_box(sqrt_target),
                black_box(sqrt_current),
            )
        })
    });

    group.finish();
}

criterion_group!(swap_benches, bench_swap_math);
criterion_main!(swap_benches);
