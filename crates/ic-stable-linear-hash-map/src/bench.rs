#[cfg(target_family = "wasm")]
use crate::StableLinearHashMap;
#[cfg(target_family = "wasm")]
use canbench_rs::bench;
#[cfg(target_family = "wasm")]
use ic_stable_structures::{DefaultMemoryImpl, StableBTreeMap};
#[cfg(target_family = "wasm")]
use std::hint::black_box;

#[cfg(target_family = "wasm")]
const ENTRY_COUNT: u64 = 4096;

#[cfg(target_family = "wasm")]
fn fixture_value(key: u64) -> u64 {
    key.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xa5a5_5a5a_a5a5_5a5a
}

#[cfg(target_family = "wasm")]
fn fixture_entries() -> Vec<(u64, u64)> {
    (0..ENTRY_COUNT)
        .map(|key| (key, fixture_value(key)))
        .collect()
}

#[cfg(target_family = "wasm")]
fn populated_linear(
    entries: &[(u64, u64)],
) -> StableLinearHashMap<u64, u64, ic_stable_structures::DefaultMemoryImpl> {
    let map = StableLinearHashMap::new(DefaultMemoryImpl::default()).expect("new linear map");
    for &(key, value) in entries {
        assert_eq!(map.insert(key, value), Ok(None));
    }
    assert_eq!(map.len(), Ok(entries.len() as u64));
    map
}

#[cfg(target_family = "wasm")]
fn populated_btree(entries: &[(u64, u64)]) -> StableBTreeMap<u64, u64, DefaultMemoryImpl> {
    let mut map = StableBTreeMap::new(DefaultMemoryImpl::default());
    for &(key, value) in entries {
        assert_eq!(map.insert(key, value), None);
    }
    assert_eq!(map.len(), entries.len() as u64);
    map
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_get_4096() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let map = populated_linear(&entries);
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("linear_get_4096");
        for &(key, value) in &entries {
            assert_eq!(black_box(map.get(&black_box(key))), Ok(Some(value)));
        }
    });
    assert_eq!(map.len(), Ok(ENTRY_COUNT));
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_btree_get_4096() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let map = populated_btree(&entries);
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("btree_get_4096");
        for &(key, value) in &entries {
            assert_eq!(black_box(map.get(&black_box(key))), Some(value));
        }
    });
    assert_eq!(map.len(), ENTRY_COUNT);
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_insert_4096() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let map = StableLinearHashMap::new(DefaultMemoryImpl::default()).expect("new linear map");
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("linear_insert_4096");
        for &(key, value) in &entries {
            assert_eq!(
                black_box(map.insert(black_box(key), black_box(value))),
                Ok(None)
            );
        }
    });
    assert_eq!(map.len(), Ok(ENTRY_COUNT));
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_btree_insert_4096() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let mut map = StableBTreeMap::new(DefaultMemoryImpl::default());
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("btree_insert_4096");
        for &(key, value) in &entries {
            assert_eq!(
                black_box(map.insert(black_box(key), black_box(value))),
                None
            );
        }
    });
    assert_eq!(map.len(), ENTRY_COUNT);
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_remove_4096() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let map = populated_linear(&entries);
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("linear_remove_4096");
        for &(key, value) in &entries {
            assert_eq!(black_box(map.remove(&black_box(key))), Ok(Some(value)));
        }
    });
    assert_eq!(map.len(), Ok(0));
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_btree_remove_4096() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let mut map = populated_btree(&entries);
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("btree_remove_4096");
        for &(key, value) in &entries {
            assert_eq!(black_box(map.remove(&black_box(key))), Some(value));
        }
    });
    assert_eq!(map.len(), 0);
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_maintenance_4096() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let map = populated_linear(&entries);
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("linear_maintenance_4096");
        black_box(map.maintenance_step(ENTRY_COUNT, 16 * 1024 * 1024)).expect("maintenance step");
    });
    assert_eq!(map.len(), Ok(ENTRY_COUNT));
    result
}
