//! Canbench micro-benchmarks for the stable clustered hash map. Run from
//! `crates/ic-stable-clustered-hash-map`: `canbench`. The `get`/`insert`/`remove` per-op cost is
//! compared against the `StableBTreeMap` baseline (the ordered-tree structure that motivated the
//! hash-map crates). The general `get`/`insert`/`remove` cases have matching B-tree cases for
//! side-by-side comparisons. Clustered-only cases isolate its resize and remap mechanics, which do
//! not have an equivalent B-tree operation.
//! The active-remap overflow case covers the full-drain fallback. A standalone settled
//! inner-overflow case remains deferred pending a separately verified fixture.

use crate::{StableClusteredHashMap, map::canbench_fixtures};
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

/// Cost of a direct insert into a nonempty, settled table with an empty home slot.
#[bench(raw)]
fn bench_clustered_insert_settled_direct() -> canbench_rs::BenchResult {
    let fixture = canbench_fixtures::settled_direct_insert();
    let target = fixture.target;

    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_clustered_insert_settled_direct");
        black_box(fixture.map.insert(target, target).expect("direct insert"));
    });

    fixture.assert_postconditions();
    result
}

/// Cost of a settled insert that relocates exactly four adjacent singleton clusters.
#[bench(raw)]
fn bench_clustered_insert_settled_relocation_chain() -> canbench_rs::BenchResult {
    let fixture = canbench_fixtures::settled_relocation_chain_insert();
    let target = fixture.target;

    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_clustered_insert_settled_relocation_chain");
        black_box(
            fixture
                .map
                .insert(target, target)
                .expect("relocation-chain insert"),
        );
    });

    fixture.assert_postconditions();
    result
}

/// Cost of one insert after an N=8 resize has started its exact 64-entry remap batch.
#[bench(raw)]
fn bench_clustered_insert_active_remap_batch() -> canbench_rs::BenchResult {
    let fixture = canbench_fixtures::active_remap_batch_insert();
    let target = fixture.target;

    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_clustered_insert_active_remap_batch");
        black_box(
            fixture
                .map
                .insert(target, target)
                .expect("active-remap insert"),
        );
    });

    fixture.assert_postconditions();
    result
}

/// Cost of the N=13 normal-load threshold resize using one key per occupied home bucket.
#[bench(raw)]
fn bench_clustered_insert_n13_threshold_resize() -> canbench_rs::BenchResult {
    let fixture = canbench_fixtures::n13_threshold_resize_insert();
    let target = fixture.target;

    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_clustered_insert_n13_threshold_resize");
        black_box(
            fixture
                .map
                .insert(target, target)
                .expect("N=13 threshold-resize insert"),
        );
    });

    fixture.assert_postconditions();
    result
}

/// Cost of active-remap overflow: the remap batch fills the terminal cluster, drains, then grows.
#[bench(raw)]
fn bench_clustered_insert_active_remap_overflow_full_drain_resize() -> canbench_rs::BenchResult {
    let fixture = canbench_fixtures::active_remap_overflow_full_drain_resize_insert();
    let target = fixture.target;

    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(
            "bench_clustered_insert_active_remap_overflow_full_drain_resize",
        );
        black_box(
            fixture
                .map
                .insert(target, target)
                .expect("active-remap overflow fallback insert"),
        );
    });

    fixture.assert_postconditions();
    result
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
