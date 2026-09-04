#![allow(dead_code, unused_imports)]

//! Benchmark suite for the initialized-tick slab.
//!
//! Run with: `cargo bench --bench slab`

use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Unbounded},
};

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};

#[path = "../src/core/mod.rs"]
mod core;
#[path = "../src/v4.rs"]
mod v4;

pub use core::error::Error;

use crate::{
    core::{
        concentrated::ticks::slab::TickSlab,
        types::{tick::TickIndex, tick_spacing::TickSpacing},
    },
    v4::{Liquidity, TickInfo, TickInfoInner},
};

const TICK_SPACING: TickSpacing = TickSpacing::MIN;
const DENSE_LIMIT: i32 = 65_535;
const DENSE_STEP: usize = 17;
const SPARSE_KEYS: [i32; 13] = [
    1, 15, 16, 17, 1_023, 1_024, 1_039, 2_047, 2_048, 16_384, 32_768, 49_152, 65_534,
];
const PROBES: [i32; 14] = [
    0, 1, 14, 15, 18, 1_022, 1_023, 1_025, 2_046, 2_049, 16_383, 32_767, 49_151, 65_533,
];

fn tick(value: i32) -> TickIndex {
    TickIndex::new(value).unwrap()
}

fn info(tick_idx: i32) -> TickInfo {
    TickInfo {
        inner: TickInfoInner {
            liquidity_gross: Liquidity::new(tick_idx as u128 + 1),
            liquidity_net: tick_idx as i128,
            ..TickInfoInner::DEFAULT
        },
        ..TickInfo::DEFAULT
    }
}

fn liquidity_gross(info: &TickInfo) -> u128 {
    info.inner.liquidity_gross.value()
}

fn build_dense_keys() -> Vec<i32> {
    (0..=DENSE_LIMIT).step_by(DENSE_STEP).collect()
}

fn build_sparse_slab() -> TickSlab {
    let mut slab = TickSlab::new(TICK_SPACING);
    for key in SPARSE_KEYS {
        slab.insert(slab.indexer(tick(key)), info(key)).unwrap();
    }
    slab
}

fn build_sparse_btree_map() -> BTreeMap<i32, TickInfo> {
    SPARSE_KEYS
        .into_iter()
        .map(|key| (key, info(key)))
        .collect()
}

fn build_dense_slab() -> (TickSlab, Vec<i32>) {
    let keys = build_dense_keys();
    let mut slab = TickSlab::new(TICK_SPACING);
    for &key in &keys {
        slab.insert(slab.indexer(tick(key)), info(key)).unwrap();
    }
    (slab, keys)
}

fn build_dense_btree_map() -> (BTreeMap<i32, TickInfo>, Vec<i32>) {
    let keys = build_dense_keys();
    let map = keys.iter().map(|&key| (key, info(key))).collect();
    (map, keys)
}

fn btree_next(map: &BTreeMap<i32, TickInfo>, key: i32) -> Option<&TickInfo> {
    map.range((Excluded(key), Unbounded))
        .next()
        .map(|(_, info)| info)
}

fn btree_prev(map: &BTreeMap<i32, TickInfo>, key: i32) -> Option<&TickInfo> {
    map.range(..key).next_back().map(|(_, info)| info)
}

fn bench_tick_slab(c: &mut Criterion) {
    let mut group = c.benchmark_group("tick_slab");

    group.bench_function("new_tick_spacing_1", |b| {
        b.iter(|| black_box(TickSlab::new(black_box(TICK_SPACING))))
    });

    let dense_keys = build_dense_keys();
    group.bench_function("insert_dense_keys", |b| {
        b.iter_batched(
            || TickSlab::new(TICK_SPACING),
            |mut slab| {
                for &key in &dense_keys {
                    black_box(
                        slab.insert(black_box(slab.indexer(tick(key))), black_box(info(key)))
                            .unwrap(),
                    );
                }
                black_box(slab)
            },
            BatchSize::SmallInput,
        )
    });

    let (dense_slab, dense_keys) = build_dense_slab();
    group.bench_function("get_hits_dense", |b| {
        b.iter(|| {
            let mut sum = 0u128;
            for &key in &dense_keys {
                sum = sum.wrapping_add(
                    dense_slab
                        .get(black_box(dense_slab.indexer(tick(key))))
                        .map(liquidity_gross)
                        .unwrap_or_default(),
                );
            }
            black_box(sum)
        })
    });

    group.bench_function("get_misses_dense", |b| {
        b.iter(|| {
            let mut misses = 0usize;
            for &key in &dense_keys {
                misses += dense_slab
                    .get(black_box(dense_slab.indexer(tick(key + 1))))
                    .is_none() as usize;
            }
            black_box(misses)
        })
    });

    let sparse_slab = build_sparse_slab();
    group.bench_function("next_sparse_boundaries", |b| {
        b.iter(|| {
            let mut sum = 0u128;
            for &probe in &PROBES {
                sum = sum.wrapping_add(
                    sparse_slab
                        .next(black_box(sparse_slab.indexer(tick(probe))))
                        .map(|(_, info)| liquidity_gross(info))
                        .unwrap_or_default(),
                );
            }
            black_box(sum)
        })
    });

    group.bench_function("prev_sparse_boundaries", |b| {
        b.iter(|| {
            let mut sum = 0u128;
            for &probe in &PROBES {
                sum = sum.wrapping_add(
                    sparse_slab
                        .prev(black_box(sparse_slab.indexer(tick(probe))))
                        .map(|(_, info)| liquidity_gross(info))
                        .unwrap_or_default(),
                );
            }
            black_box(sum)
        })
    });

    group.bench_function("remove_insert_cycle_sparse", |b| {
        b.iter_batched(
            build_sparse_slab,
            |mut slab| {
                for &key in &SPARSE_KEYS {
                    let indexer = slab.indexer(tick(key));
                    slab.remove(black_box(indexer));
                    black_box(());
                    black_box(
                        slab.insert(black_box(indexer), black_box(info(key)))
                            .unwrap(),
                    );
                }
                black_box(slab)
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn bench_btree_map(c: &mut Criterion) {
    let mut group = c.benchmark_group("btree_map_tick_info");

    group.bench_function("new", |b| {
        b.iter(|| black_box(BTreeMap::<i32, TickInfo>::new()))
    });

    let dense_keys = build_dense_keys();
    group.bench_function("insert_dense_keys", |b| {
        b.iter_batched(
            BTreeMap::new,
            |mut map| {
                for &key in &dense_keys {
                    black_box(map.insert(black_box(key), black_box(info(key))));
                }
                black_box(map)
            },
            BatchSize::SmallInput,
        )
    });

    let (dense_map, dense_keys) = build_dense_btree_map();
    group.bench_function("get_hits_dense", |b| {
        b.iter(|| {
            let mut sum = 0u128;
            for &key in &dense_keys {
                sum = sum.wrapping_add(
                    dense_map
                        .get(&black_box(key))
                        .map(liquidity_gross)
                        .unwrap_or_default(),
                );
            }
            black_box(sum)
        })
    });

    group.bench_function("get_misses_dense", |b| {
        b.iter(|| {
            let mut misses = 0usize;
            for &key in &dense_keys {
                misses += (!dense_map.contains_key(&black_box(key + 1))) as usize;
            }
            black_box(misses)
        })
    });

    let sparse_map = build_sparse_btree_map();
    group.bench_function("next_sparse_boundaries", |b| {
        b.iter(|| {
            let mut sum = 0u128;
            for &probe in &PROBES {
                sum = sum.wrapping_add(
                    btree_next(&sparse_map, black_box(probe))
                        .map(liquidity_gross)
                        .unwrap_or_default(),
                );
            }
            black_box(sum)
        })
    });

    group.bench_function("prev_sparse_boundaries", |b| {
        b.iter(|| {
            let mut sum = 0u128;
            for &probe in &PROBES {
                sum = sum.wrapping_add(
                    btree_prev(&sparse_map, black_box(probe))
                        .map(liquidity_gross)
                        .unwrap_or_default(),
                );
            }
            black_box(sum)
        })
    });

    group.bench_function("remove_insert_cycle_sparse", |b| {
        b.iter_batched(
            build_sparse_btree_map,
            |mut map| {
                for &key in &SPARSE_KEYS {
                    black_box(map.remove(&black_box(key)));
                    black_box(map.insert(black_box(key), black_box(info(key))));
                }
                black_box(map)
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(slab_benches, bench_tick_slab, bench_btree_map);
criterion_main!(slab_benches);
