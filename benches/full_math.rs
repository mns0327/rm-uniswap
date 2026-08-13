//! Benchmark suite for full-width multiplication and division math.
//!
//! Run with: `cargo bench --bench full_math`

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rm_uniswap::v4::full_math;
use ruint::aliases::U256;

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

criterion_group!(full_math_benches, bench_full_math);
criterion_main!(full_math_benches);
