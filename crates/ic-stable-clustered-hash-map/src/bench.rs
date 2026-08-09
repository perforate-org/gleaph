//! Canbench micro-benchmarks for the stable clustered hash map. Run from
//! `crates/ic-stable-clustered-hash-map`: `canbench`. The `get`/`insert`/`remove` per-op cost is
//! compared against the ~105K BTreeMap baseline that motivated the hash-map crates.

use crate::StableClusteredHashMap;
use canbench_rs::bench;
use ic_stable_structures::VectorMemory;
use std::hint::black_box;

const N: u64 = 4096;

fn setup() -> StableClusteredHashMap<u64, u64, VectorMemory> {
    let map = StableClusteredHashMap::new(VectorMemory::default()).expect("new");
    for k in 0..N {
        map.insert(k, k).expect("insert");
    }
    map
}

/// Cost of `N` point lookups over a populated map. Per-get = total / N.
#[bench(raw)]
fn bench_clustered_get() -> canbench_rs::BenchResult {
    let map = setup();
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

/// Cost of `N` removes from a populated map (cluster tail-fill).
#[bench(raw)]
fn bench_clustered_remove() -> canbench_rs::BenchResult {
    let map = setup();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_clustered_remove");
        for k in 0..N {
            map.remove(&k);
        }
    })
}
