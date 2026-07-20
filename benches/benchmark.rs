//! Benchmark suite for Uniswap V4 AMM math and swap simulation.
//!
//! Run with: `cargo bench --package rm-uniswap --all-features`
//!
//! ## Structure
//!
//! Each group measures a hot path used by V4 math, quoting, or full pool
//! simulation.

use std::path::Path;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rm_uniswap::v4::{
    ModifyLiquidityParams, Pool, SignedAmount, SwapParams, full_math, sqrt_price_math, swap_math,
    tick_math,
};
use ruint::aliases::U256;

fn build_test_pool() -> Pool {
    let path = Path::new("samples/pool1.json");
    let file = std::fs::File::open(path).unwrap();
    let pool: Pool = serde_json::from_reader(file).unwrap();
    pool
}

fn sqrt_price_1_1() -> U256 {
    // √(1/1) · 2^96
    U256::ONE << 96
}

fn ether(n: u128) -> U256 {
    U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
}

fn liq(n: u128) -> U256 {
    U256::from(n)
}

fn bench_full_math(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_math");

    let inputs = (
        U256::from(1_000_000_000_000_000_000u128),
        U256::from(3_500_000_000_000_000_000u128),
        U256::from(1_000_000_000_000_000_000u128),
    );

    group.bench_function("FullMath::mul_div", |b| {
        b.iter(|| black_box(full_math::mul_div(inputs.0, inputs.1, inputs.2)))
    });

    group.bench_function("FullMath::mul_div_rounding_up", |b| {
        b.iter(|| black_box(full_math::mul_div_rounding_up(inputs.0, inputs.1, inputs.2)))
    });

    // Phantom-overflow edge case: a*b exceeds U256::MAX but quotient fits
    let q128 = U256::ONE << 128;
    group.bench_function("mul_div_phantom_overflow", |b| {
        b.iter(|| {
            black_box(full_math::mul_div(
                q128,
                U256::from(35) * q128,
                U256::from(8) * q128,
            ))
        })
    });

    group.finish();
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
        let sqrt_b = tick_math::get_sqrt_price_at_tick(60).unwrap();
        b.iter(|| {
            black_box(sqrt_price_math::get_amount0_delta(
                sqrt_a, sqrt_b, liquidity, true,
            ))
        })
    });

    group.bench_function("sqrt_price_math::get_amount1_delta", |b| {
        let sqrt_a = sqrt_price_1_1();
        let sqrt_b = tick_math::get_sqrt_price_at_tick(60).unwrap();
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

fn bench_tick_math(c: &mut Criterion) {
    let mut group = c.benchmark_group("tick_math");

    group.bench_function("tick_math::get_sqrt_price_at_tick", |b| {
        b.iter(|| black_box(tick_math::get_sqrt_price_at_tick(60)).unwrap())
    });

    group.bench_function("get_sqrt_price_at_tick_min", |b| {
        b.iter(|| black_box(tick_math::get_sqrt_price_at_tick(tick_math::MIN_TICK)).unwrap())
    });

    group.bench_function("get_sqrt_price_at_tick_max", |b| {
        b.iter(|| black_box(tick_math::get_sqrt_price_at_tick(tick_math::MAX_TICK)).unwrap())
    });

    let sqrt_p = tick_math::get_sqrt_price_at_tick(60).unwrap();
    group.bench_function("tick_math::get_tick_at_sqrt_price", |b| {
        b.iter(|| black_box(tick_math::get_tick_at_sqrt_price(sqrt_p)).unwrap())
    });

    group.bench_function("get_tick_at_sqrt_price_min", |b| {
        b.iter(|| black_box(tick_math::get_tick_at_sqrt_price(tick_math::MIN_SQRT_PRICE)).unwrap())
    });

    group.bench_function("get_tick_at_sqrt_price_max", |b| {
        // Just below max to stay in valid range
        let almost_max = tick_math::MAX_SQRT_PRICE - U256::ONE;
        b.iter(|| black_box(tick_math::get_tick_at_sqrt_price(almost_max)).unwrap())
    });

    group.bench_function("tick_math::most_significant_bit", |b| {
        b.iter(|| black_box(tick_math::most_significant_bit(U256::from(1u64) << 200)).unwrap())
    });

    group.bench_function("most_significant_bit_max", |b| {
        b.iter(|| black_box(tick_math::most_significant_bit(U256::MAX)).unwrap())
    });

    group.bench_function("tick_math::max_usable_tick", |b| {
        b.iter(|| black_box(tick_math::max_usable_tick(60)).unwrap())
    });

    group.bench_function("tick_math::min_usable_tick", |b| {
        b.iter(|| black_box(tick_math::min_usable_tick(60)).unwrap())
    });

    group.finish();
}

fn bench_swap_math(c: &mut Criterion) {
    let mut group = c.benchmark_group("swap_math");

    let sqrt_current = sqrt_price_1_1();
    let sqrt_target = tick_math::get_sqrt_price_at_tick(60).unwrap();
    let liquidity = 1_000_000_000_000_000_000u128;
    let fee = 3000u32;

    group.bench_function("compute_swap_step_exact_in", |b| {
        b.iter(|| {
            swap_math::compute_swap_step(
                black_box(sqrt_current),
                black_box(sqrt_target),
                black_box(liquidity),
                black_box(SignedAmount::negative(ether(1))),
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
                black_box(SignedAmount::positive(ether(1))),
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

fn bench_signed_amount(c: &mut Criterion) {
    let mut group = c.benchmark_group("signed_amount");

    let pos = SignedAmount::positive(ether(100));
    let neg = SignedAmount::negative(ether(100));

    group.bench_function("neg", |b| b.iter(|| black_box(pos).neg()));

    group.bench_function("checked_add", |b| {
        b.iter(|| black_box(pos).checked_add(black_box(neg)))
    });

    group.bench_function("checked_sub", |b| {
        b.iter(|| black_box(pos).checked_sub(black_box(neg)))
    });

    group.bench_function("add_unsigned", |b| {
        let mut amt = black_box(SignedAmount::negative(ether(100)));
        b.iter(|| amt.add_unsigned(ether(50)).unwrap())
    });

    group.bench_function("sub_unsigned", |b| {
        let mut amt = black_box(SignedAmount::positive(ether(100)));
        b.iter(|| amt.sub_unsigned(ether(50)).unwrap())
    });

    group.bench_function("from_i128", |b| {
        b.iter(|| black_box(SignedAmount::from(-42i128)))
    });

    group.bench_function("abs", |b| b.iter(|| black_box(neg).abs()));

    group.finish();
}

fn bench_pool_ticks(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_ticks");

    let pool = build_test_pool();
    let pool_ticks = pool.ticks;

    let tick_lower = pool_ticks.read().first_tick();
    let tick_upper = pool_ticks.read().last_tick();

    // next_initialized_tick — O(log n) BTreeMap lookup
    group.bench_function("next_initialized_tick_zero_for_one", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let guard = pool.read();
            guard.next_initialized_tick(0, true)
        })
    });

    group.bench_function("next_initialized_tick_one_for_zero", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let guard = pool.read();
            guard.next_initialized_tick(0, false)
        })
    });

    // cross_tick — O(1) map lookup
    group.bench_function("cross_tick", |b| {
        let pool = pool_ticks.clone();
        b.iter(|| {
            let guard = pool.read();
            guard.cross_tick(tick_upper).unwrap()
        })
    });

    // write path — update_tick
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

    // pool.simulate_swap — read-only full tick-crossing loop
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

    // pool.quote_exact_input — convenience wrapper
    group.bench_function("quote_exact_input", |b| {
        b.iter(|| {
            pool.quote_exact_input(black_box(ether(1)), black_box(true))
                .unwrap()
        })
    });

    // pool.swap — commits state, measures overhead of state mutation
    group.bench_function("swap_exact_in", |b| {
        b.iter(|| {
            pool.swap(SwapParams::new(
                black_box(true),
                black_box(SignedAmount::negative(ether(1))),
            ))
        })
    });

    // pool.modify_liquidity
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

    // tick_spacing_to_max_liquidity_per_tick — pure computation
    // Inlined here since it's a simple utility. Signature:
    // pub fn tick_spacing_to_max_liquidity_per_tick(tick_spacing: i32) -> u128
    group.bench_function("tick_spacing_to_max_liquidity_per_tick", |b| {
        b.iter(|| {
            let ts = black_box(60i32);
            let min_compressed = -887272i32.div_euclid(ts);
            let max_compressed = 887272i32.div_euclid(ts);
            let num_ticks = (max_compressed - min_compressed + 1) as u128;
            black_box(u128::MAX / num_ticks)
        })
    });

    group.finish();
}

criterion_group!(
    all_benches,
    bench_full_math,
    bench_sqrt_price_math,
    bench_tick_math,
    bench_swap_math,
    bench_signed_amount,
    bench_pool_ticks,
    bench_pool,
    bench_utils,
);
criterion_main!(all_benches);
