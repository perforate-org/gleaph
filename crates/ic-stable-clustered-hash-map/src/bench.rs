//! Canbench micro-benchmarks for the stable clustered hash map. Run from
//! `crates/ic-stable-clustered-hash-map`: `canbench`. The `get`/`insert`/`remove` per-op cost is
//! compared against the `StableBTreeMap` baseline (the ordered-tree structure that motivated the
//! hash-map crates). Every `bench_*_clustered_*` case has a matching `bench_*_btree_*` case so the
//! performance difference can be compared side by side.

use crate::StableClusteredHashMap;
use canbench_rs::bench;
use ic_stable_structures::{StableBTreeMap, VectorMemory};
use std::hint::black_box;

const N: u64 = 4096;

fn setup_clustered() -> StableClusteredHashMap<u64, u64, VectorMemory> {
    let map = StableClusteredHashMap::new(VectorMemory::default()).expect("new");
    for k in 0..N {
        map.insert(k, k).expect("insert");
    }
    map
}

fn setup_btree() -> StableBTreeMap<u64, u64, VectorMemory> {
    let mut map = StableBTreeMap::init(VectorMemory::default());
    for k in 0..N {
        map.insert(k, k);
    }
    map
}

/// Cost of `N` point lookups over a populated clustered map. Per-get = total / N.
#[bench(raw)]
fn bench_clustered_get() -> canbench_rs::BenchResult {
    let map = setup_clustered();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_clustered_get");
        let mut sum = 0u64;
        for k in 0..N {
            if let Some(v) = map.get(&k) {
                sum = sum.wrapping_add(v);
            }
        }
        black_box(sum);
    })
}

/// Cost of `N` point lookups over a populated `StableBTreeMap`. Per-get = total / N.
#[bench(raw)]
fn bench_btree_get() -> canbench_rs::BenchResult {
    let map = setup_btree();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_btree_get");
        let mut sum = 0u64;
        for k in 0..N {
            if let Some(v) = map.get(&k) {
                sum = sum.wrapping_add(v);
            }
        }
        black_box(sum);
    })
}

/// Cost of `N` inserts into a fresh map (includes resizes).
#[bench(raw)]
fn bench_clustered_insert() -> canbench_rs::BenchResult {
    let map = StableClusteredHashMap::new(VectorMemory::default()).expect("new");
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_clustered_insert");
        for k in 0..N {
            map.insert(k, k).expect("insert");
        }
    })
}

/// Cost of `N` inserts into a fresh `StableBTreeMap` (includes rebalancing).
#[bench(raw)]
fn bench_btree_insert() -> canbench_rs::BenchResult {
    let mut map = StableBTreeMap::init(VectorMemory::default());
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_btree_insert");
        for k in 0..N {
            map.insert(k, k);
        }
    })
}

/// Cost of `N` removes from a populated clustered map (cluster tail-fill).
#[bench(raw)]
fn bench_clustered_remove() -> canbench_rs::BenchResult {
    let map = setup_clustered();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_clustered_remove");
        for k in 0..N {
            map.remove(&k);
        }
    })
}

/// Cost of `N` removes from a populated `StableBTreeMap`.
#[bench(raw)]
fn bench_btree_remove() -> canbench_rs::BenchResult {
    let mut map = setup_btree();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_btree_remove");
        for k in 0..N {
            map.remove(&k);
        }
    })
}
