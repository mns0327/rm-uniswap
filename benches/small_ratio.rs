//! Benchmark suite for exact `U256 * u32 / u32` fee-ratio arithmetic.
//!
//! Run with: `cargo bench --bench small_ratio`

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use ruint::aliases::U256;

mod full {
    pub type MathError = rm_uniswap::Error;
}

#[path = "../src/core/math/small_ratio.rs"]
mod small_ratio;

const MAX_SWAP_FEE: u32 = 1_000_000;
const FEE_LOW: u32 = 500;
const FEE_MEDIUM: u32 = 3_000;
const FEE_HIGH: u32 = 10_000;

fn ether(n: u128) -> U256 {
    U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
}

fn generic_floor(amount: U256, numerator: u32, denominator: u32) -> U256 {
    amount * U256::from(numerator) / U256::from(denominator)
}

fn generic_ceil(amount: U256, numerator: u32, denominator: u32) -> U256 {
    let numerator = amount * U256::from(numerator);
    let denominator = U256::from(denominator);
    let quotient = numerator / denominator;

    if numerator % denominator == U256::ZERO {
        quotient
    } else {
        quotient + U256::ONE
    }
}

fn bench_small_ratio(c: &mut Criterion) {
    let mut group = c.benchmark_group("small_ratio");

    let low64_amount = U256::from(1_000_000_000_000u64);
    let low128_amount = ether(1_000_000_000);
    let wide_amount = U256::MAX >> 32;

    for (name, amount) in [
        ("low64", low64_amount),
        ("low128", low128_amount),
        ("wide", wide_amount),
    ] {
        group.bench_function(format!("floor/{name}/fee_medium"), |b| {
            b.iter(|| {
                black_box(small_ratio::mul_div_u32_floor(
                    black_box(amount),
                    black_box(MAX_SWAP_FEE - FEE_MEDIUM),
                    black_box(MAX_SWAP_FEE),
                ))
            })
        });

        group.bench_function(format!("ceil/{name}/fee_medium"), |b| {
            b.iter(|| {
                black_box(small_ratio::mul_div_u32_ceil(
                    black_box(amount),
                    black_box(FEE_MEDIUM),
                    black_box(MAX_SWAP_FEE - FEE_MEDIUM),
                ))
            })
        });
    }

    for (name, fee) in [
        ("fee_low", FEE_LOW),
        ("fee_medium", FEE_MEDIUM),
        ("fee_high", FEE_HIGH),
    ] {
        group.bench_function(format!("floor/optimized/low128/{name}"), |b| {
            b.iter(|| {
                black_box(small_ratio::mul_div_u32_floor(
                    black_box(low128_amount),
                    black_box(MAX_SWAP_FEE - fee),
                    black_box(MAX_SWAP_FEE),
                ))
            })
        });

        group.bench_function(format!("floor/generic/low128/{name}"), |b| {
            b.iter(|| {
                black_box(generic_floor(
                    black_box(low128_amount),
                    black_box(MAX_SWAP_FEE - fee),
                    black_box(MAX_SWAP_FEE),
                ))
            })
        });

        group.bench_function(format!("ceil/optimized/low128/{name}"), |b| {
            b.iter(|| {
                black_box(small_ratio::mul_div_u32_ceil(
                    black_box(low128_amount),
                    black_box(fee),
                    black_box(MAX_SWAP_FEE - fee),
                ))
            })
        });

        group.bench_function(format!("ceil/generic/low128/{name}"), |b| {
            b.iter(|| {
                black_box(generic_ceil(
                    black_box(low128_amount),
                    black_box(fee),
                    black_box(MAX_SWAP_FEE - fee),
                ))
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_small_ratio);
criterion_main!(benches);
