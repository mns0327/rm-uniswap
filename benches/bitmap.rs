//! Benchmark suite for the hierarchical initialized-tick bitmap.
//!
//! Run with: `cargo bench --bench bitmap`

use std::{
    collections::BTreeSet,
    ops::Bound::{Excluded, Unbounded},
};

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};

#[allow(dead_code)]
#[path = "../src/core/concentrated/ticks/bitmap.rs"]
mod bitmap;

use bitmap::HierBitmap;

const BITMAP_CAP: u16 = u16::MAX;
const SPARSE_KEYS: [u16; 13] = [
    1, 63, 64, 65, 4_095, 4_096, 4_159, 8_191, 8_192, 16_384, 32_768, 49_152, 65_534,
];
const PROBES: [u16; 14] = [
    0, 1, 62, 63, 66, 4_094, 4_095, 4_097, 8_190, 8_193, 16_383, 32_767, 49_151, 65_533,
];

fn build_sparse_bitmap() -> HierBitmap {
    let mut bitmap = HierBitmap::new(BITMAP_CAP);
    for key in SPARSE_KEYS {
        bitmap.set(key);
    }
    bitmap
}

fn build_sparse_btree_set() -> BTreeSet<u16> {
    SPARSE_KEYS.into_iter().collect()
}

fn build_dense_keys() -> Vec<u16> {
    (0..BITMAP_CAP).step_by(17).collect()
}

fn build_dense_bitmap() -> (HierBitmap, Vec<u16>) {
    let keys = build_dense_keys();
    let mut bitmap = HierBitmap::new(BITMAP_CAP);
    for &key in &keys {
        bitmap.set(key);
    }
    (bitmap, keys)
}

fn build_dense_btree_set() -> (BTreeSet<u16>, Vec<u16>) {
    let keys = build_dense_keys();
    let set = keys.iter().copied().collect();
    (set, keys)
}

fn btree_next(set: &BTreeSet<u16>, key: u16) -> Option<u16> {
    set.range((Excluded(key), Unbounded)).next().copied()
}

fn btree_next_or_eq(set: &BTreeSet<u16>, key: u16) -> Option<u16> {
    set.range(key..).next().copied()
}

fn btree_prev(set: &BTreeSet<u16>, key: u16) -> Option<u16> {
    set.range(..key).next_back().copied()
}

fn btree_prev_or_eq(set: &BTreeSet<u16>, key: u16) -> Option<u16> {
    set.range(..=key).next_back().copied()
}

fn bench_hier_bitmap(c: &mut Criterion) {
    let mut group = c.benchmark_group("hier_bitmap");

    group.bench_function("new_max_cap", |b| {
        b.iter(|| black_box(HierBitmap::new(black_box(BITMAP_CAP))))
    });

    let dense_keys = build_dense_keys();
    group.bench_function("set_dense_keys", |b| {
        b.iter_batched(
            || HierBitmap::new(BITMAP_CAP),
            |mut bitmap| {
                for &key in &dense_keys {
                    black_box(bitmap.set(black_box(key)));
                }
                black_box(bitmap)
            },
            BatchSize::SmallInput,
        )
    });

    let (dense_bitmap, dense_keys) = build_dense_bitmap();
    group.bench_function("contains_hits_dense", |b| {
        b.iter(|| {
            let mut hits = 0usize;
            for &key in &dense_keys {
                hits += dense_bitmap.contains(black_box(key)) as usize;
            }
            black_box(hits)
        })
    });

    group.bench_function("contains_misses_dense", |b| {
        b.iter(|| {
            let mut misses = 0usize;
            for &key in &dense_keys {
                misses += (!dense_bitmap.contains(black_box(key.saturating_add(1)))) as usize;
            }
            black_box(misses)
        })
    });

    let sparse_bitmap = build_sparse_bitmap();
    group.bench_function("next_sparse_boundaries", |b| {
        b.iter(|| {
            let mut found = 0u32;
            for &probe in &PROBES {
                found = found.wrapping_add(
                    sparse_bitmap
                        .next(black_box(probe))
                        .unwrap_or_default()
                        .into(),
                );
            }
            black_box(found)
        })
    });

    group.bench_function("next_or_eq_sparse_boundaries", |b| {
        b.iter(|| {
            let mut found = 0u32;
            for &probe in &PROBES {
                found = found.wrapping_add(
                    sparse_bitmap
                        .next_or_eq(black_box(probe))
                        .unwrap_or_default()
                        .into(),
                );
            }
            black_box(found)
        })
    });

    group.bench_function("prev_sparse_boundaries", |b| {
        b.iter(|| {
            let mut found = 0u32;
            for &probe in &PROBES {
                found = found.wrapping_add(
                    sparse_bitmap
                        .prev(black_box(probe))
                        .unwrap_or_default()
                        .into(),
                );
            }
            black_box(found)
        })
    });

    group.bench_function("prev_or_eq_sparse_boundaries", |b| {
        b.iter(|| {
            let mut found = 0u32;
            for &probe in &PROBES {
                found = found.wrapping_add(
                    sparse_bitmap
                        .prev_or_eq(black_box(probe))
                        .unwrap_or_default()
                        .into(),
                );
            }
            black_box(found)
        })
    });

    group.bench_function("remove_set_cycle_sparse", |b| {
        b.iter_batched(
            build_sparse_bitmap,
            |mut bitmap| {
                for &key in &SPARSE_KEYS {
                    black_box(bitmap.remove(black_box(key)));
                    black_box(bitmap.set(black_box(key)));
                }
                black_box(bitmap)
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn bench_btree_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("btree_set");

    group.bench_function("new", |b| b.iter(|| black_box(BTreeSet::<u16>::new())));

    let dense_keys = build_dense_keys();
    group.bench_function("insert_dense_keys", |b| {
        b.iter_batched(
            BTreeSet::new,
            |mut set| {
                for &key in &dense_keys {
                    black_box(set.insert(black_box(key)));
                }
                black_box(set)
            },
            BatchSize::SmallInput,
        )
    });

    let (dense_set, dense_keys) = build_dense_btree_set();
    group.bench_function("contains_hits_dense", |b| {
        b.iter(|| {
            let mut hits = 0usize;
            for &key in &dense_keys {
                hits += dense_set.contains(&black_box(key)) as usize;
            }
            black_box(hits)
        })
    });

    group.bench_function("contains_misses_dense", |b| {
        b.iter(|| {
            let mut misses = 0usize;
            for &key in &dense_keys {
                misses += (!dense_set.contains(&black_box(key.saturating_add(1)))) as usize;
            }
            black_box(misses)
        })
    });

    let sparse_set = build_sparse_btree_set();
    group.bench_function("next_sparse_boundaries", |b| {
        b.iter(|| {
            let mut found = 0u32;
            for &probe in &PROBES {
                found = found.wrapping_add(
                    btree_next(&sparse_set, black_box(probe))
                        .unwrap_or_default()
                        .into(),
                );
            }
            black_box(found)
        })
    });

    group.bench_function("next_or_eq_sparse_boundaries", |b| {
        b.iter(|| {
            let mut found = 0u32;
            for &probe in &PROBES {
                found = found.wrapping_add(
                    btree_next_or_eq(&sparse_set, black_box(probe))
                        .unwrap_or_default()
                        .into(),
                );
            }
            black_box(found)
        })
    });

    group.bench_function("prev_sparse_boundaries", |b| {
        b.iter(|| {
            let mut found = 0u32;
            for &probe in &PROBES {
                found = found.wrapping_add(
                    btree_prev(&sparse_set, black_box(probe))
                        .unwrap_or_default()
                        .into(),
                );
            }
            black_box(found)
        })
    });

    group.bench_function("prev_or_eq_sparse_boundaries", |b| {
        b.iter(|| {
            let mut found = 0u32;
            for &probe in &PROBES {
                found = found.wrapping_add(
                    btree_prev_or_eq(&sparse_set, black_box(probe))
                        .unwrap_or_default()
                        .into(),
                );
            }
            black_box(found)
        })
    });

    group.bench_function("remove_insert_cycle_sparse", |b| {
        b.iter_batched(
            build_sparse_btree_set,
            |mut set| {
                for &key in &SPARSE_KEYS {
                    black_box(set.remove(&black_box(key)));
                    black_box(set.insert(black_box(key)));
                }
                black_box(set)
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(bitmap_benches, bench_hier_bitmap, bench_btree_set);
criterion_main!(bitmap_benches);
