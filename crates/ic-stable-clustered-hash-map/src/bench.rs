//! Canbench micro-benchmarks for the stable clustered hash map. Run from
//! `crates/ic-stable-clustered-hash-map`: `canbench`. The `get`/`insert`/`remove` per-op cost is
//! compared against the `StableBTreeMap` baseline (the ordered-tree structure that motivated the
//! hash-map crates). The general `get`/`insert`/`remove` cases have matching B-tree cases for
//! side-by-side comparisons. Clustered-only cases isolate its resize and remap mechanics, which do
//! not have an equivalent B-tree operation.
//! The active-remap tail case measures logical tail extension without bucket growth or a remap
//! drain. A standalone settled inner-overflow case remains deferred pending a verified fixture.
//! The N=13/N=16/N=20/N=23 threshold cases isolate one bounded settled-table initialization step;
//! their large resident tables are constructed before the timed closure.

use crate::{StableClusteredHashMap, map::canbench_fixtures};
use canbench_rs::bench;
use ic_stable_structures::{DefaultMemoryImpl, StableBTreeMap};
use std::hint::black_box;

const N: u64 = 4096;

fn setup_clustered() -> StableClusteredHashMap<u64, u64, DefaultMemoryImpl> {
    let map = StableClusteredHashMap::new(DefaultMemoryImpl::default()).expect("new");
    for k in 0..N {
        map.insert(k, k).expect("insert");
    }
    map
}

fn setup_btree() -> StableBTreeMap<u64, u64, DefaultMemoryImpl> {
    let mut map = StableBTreeMap::init(DefaultMemoryImpl::default());
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
    let map = StableClusteredHashMap::new(DefaultMemoryImpl::default()).expect("new");
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

/// Cost of one insert that examines an exact 64-position batch of an active N=8 remap.
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

fn bench_clustered_insert_threshold_scale(
    log2_buckets: u8,
    scope_name: &'static str,
) -> canbench_rs::BenchResult {
    let fixture = canbench_fixtures::threshold_resize_insert_at(log2_buckets);
    let target = fixture.target;

    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope_name);
        black_box(
            fixture
                .map
                .insert(target, target)
                .expect("scale threshold-resize insert"),
        );
    });

    fixture.assert_postconditions();
    result
}

/// Cost of the settled threshold resize at N=16 (49,152 residents).
#[bench(raw)]
fn bench_clustered_insert_n16_threshold_resize() -> canbench_rs::BenchResult {
    bench_clustered_insert_threshold_scale(16, "bench_clustered_insert_n16_threshold_resize")
}

/// Cost of the settled threshold resize at N=20 (786,432 residents).
#[bench(raw)]
fn bench_clustered_insert_n20_threshold_resize() -> canbench_rs::BenchResult {
    bench_clustered_insert_threshold_scale(20, "bench_clustered_insert_n20_threshold_resize")
}

/// Cost of the settled threshold resize at N=23 (6,291,456 residents).
#[bench(raw)]
fn bench_clustered_insert_n23_threshold_resize() -> canbench_rs::BenchResult {
    bench_clustered_insert_threshold_scale(23, "bench_clustered_insert_n23_threshold_resize")
}

/// Cost of extending a full logical tail while preserving the active remap and bucket count.
#[bench(raw)]
fn bench_clustered_insert_active_remap_tail_extension() -> canbench_rs::BenchResult {
    let fixture = canbench_fixtures::active_remap_tail_extension_insert();
    let target = fixture.target;

    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_clustered_insert_active_remap_tail_extension");
        black_box(
            fixture
                .map
                .insert(target, target)
                .expect("active-remap tail-extension insert"),
        );
    });

    fixture.assert_postconditions();
    result
}

/// Cost of `N` inserts into a fresh `StableBTreeMap` (includes rebalancing).
#[bench(raw)]
fn bench_btree_insert() -> canbench_rs::BenchResult {
    let mut map = StableBTreeMap::init(DefaultMemoryImpl::default());
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
            map.remove(&k).expect("remove");
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
