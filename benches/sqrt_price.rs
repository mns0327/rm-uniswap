//! Benchmark suite for sqrt-price math.
//!
//! Run with: `cargo bench --bench sqrt_price`

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rm_uniswap::v4::{sqrt_price_math, tick_math, NonZeroLiquidity, SqrtPriceX96, TickIndex};
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

fn liq(n: u128) -> NonZeroLiquidity {
    NonZeroLiquidity::new(n).unwrap()
}

fn bench_sqrt_price_math(c: &mut Criterion) {
    let mut group = c.benchmark_group("sqrt_price_math");

    let sqrt_p = sqrt_price_1_1();
    let liquidity = liq(1_000_000_000_000_000_000u128);
    let amount = ether(1);

    group.bench_function(
        "sqrt_price_math::get_next_sqrt_price_from_amount0_rounding_up",
        |b| {
            b.iter(|| {
                black_box(
                    sqrt_price_math::get_next_sqrt_price_from_amount0_rounding_up(
                        sqrt_p, liquidity, amount, true,
                    ),
                )
            })
        },
    );

    group.bench_function(
        "sqrt_price_math::get_next_sqrt_price_from_amount1_rounding_down",
        |b| {
            b.iter(|| {
                black_box(
                    sqrt_price_math::get_next_sqrt_price_from_amount1_rounding_down(
                        sqrt_p, liquidity, amount, true,
                    ),
                )
            })
        },
    );

    group.bench_function("sqrt_price_math::get_amount0_delta", |b| {
        let sqrt_a = sqrt_price_1_1();
        let sqrt_b = tick_math::get_sqrt_price_at_tick(tick(60));
        b.iter(|| {
            black_box(sqrt_price_math::get_amount0_delta(
                sqrt_a, sqrt_b, liquidity, true,
            ))
        })
    });

    group.bench_function("sqrt_price_math::get_amount1_delta", |b| {
        let sqrt_a = sqrt_price_1_1();
        let sqrt_b = tick_math::get_sqrt_price_at_tick(tick(60));
        b.iter(|| {
            black_box(sqrt_price_math::get_amount1_delta(
                sqrt_a, sqrt_b, liquidity, true,
            ))
        })
    });

    group.bench_function("sqrt_price_math::get_next_sqrt_price_from_input", |b| {
        b.iter(|| {
            black_box(sqrt_price_math::get_next_sqrt_price_from_input(
                sqrt_p, liquidity, amount, true,
            ))
        })
    });

    group.bench_function("sqrt_price_math::get_next_sqrt_price_from_output", |b| {
        b.iter(|| {
            black_box(sqrt_price_math::get_next_sqrt_price_from_output(
                sqrt_p, liquidity, amount, true,
            ))
        })
    });

    group.finish();
}

criterion_group!(sqrt_price_benches, bench_sqrt_price_math);
criterion_main!(sqrt_price_benches);
