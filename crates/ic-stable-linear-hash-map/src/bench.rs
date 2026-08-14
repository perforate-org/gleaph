use crate::StableLinearHashMap;
#[cfg(target_family = "wasm")]
use crate::StableMapValue;
use canbench_rs::bench;
#[cfg(target_family = "wasm")]
use ic_stable_structures::Storable;
#[cfg(target_family = "wasm")]
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager};
use ic_stable_structures::{DefaultMemoryImpl, StableBTreeMap};
#[cfg(target_family = "wasm")]
use std::cell::RefCell;
use std::hint::black_box;
#[cfg(target_family = "wasm")]
use std::rc::Rc;

const N: usize = 48;
const LARGE_N: u64 = 16;
#[cfg(target_family = "wasm")]
const SPLIT_SEED: u64 = 211;
#[cfg(target_family = "wasm")]
const SPLIT_MOVE_0: [u64; 49] = [
    29, 60, 118, 162, 229, 318, 354, 365, 84, 114, 197, 206, 262, 339, 107, 122, 376, 416, 605,
    622, 61, 71, 130, 198, 381, 384, 39, 42, 91, 110, 132, 246, 7, 28, 69, 83, 101, 108, 98, 131,
    217, 235, 281, 38, 173, 223, 237, 240, 215,
];
#[cfg(target_family = "wasm")]
const SPLIT_MOVE_4: [u64; 49] = [
    215, 265, 887, 1017, 29, 60, 118, 162, 84, 114, 197, 206, 262, 339, 107, 122, 376, 416, 605,
    622, 61, 71, 130, 198, 381, 384, 39, 42, 91, 110, 132, 246, 7, 28, 69, 83, 101, 108, 98, 131,
    217, 235, 281, 38, 173, 223, 237, 240, 1313,
];
#[cfg(target_family = "wasm")]
const SPLIT_MOVE_8: [u64; 49] = [
    215, 265, 887, 1017, 1313, 1445, 1718, 1758, 84, 114, 197, 206, 262, 339, 107, 122, 376, 416,
    605, 622, 61, 71, 130, 198, 381, 384, 39, 42, 91, 110, 132, 246, 7, 28, 69, 83, 101, 108, 98,
    131, 217, 235, 281, 38, 173, 223, 237, 240, 118,
];
#[cfg(target_family = "wasm")]
const SPLIT_ROLLOVER: [u64; 91] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49,
    50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73,
    74, 75, 77, 78, 80, 81, 82, 83, 84, 86, 87, 88, 89, 90, 91, 92, 76,
];

fn fixture_value(key: u64) -> u64 {
    key ^ 0xa5a5
}

fn fixture_entries() -> Vec<(u64, u64)> {
    let probe = StableLinearHashMap::new(DefaultMemoryImpl::default()).expect("new probe map");
    let mut entries = Vec::with_capacity(N);
    for candidate in 0u64.. {
        let value = fixture_value(candidate);
        match probe.insert(candidate, value) {
            Ok(None) => {
                entries.push((candidate, value));
                if entries.len() == N {
                    break;
                }
            }
            Ok(Some(_)) => panic!("fixture candidates must be unique"),
            Err(_) => {}
        }
    }
    assert_eq!(entries.len(), N);
    entries
}

fn populated_linear(entries: &[(u64, u64)]) -> StableLinearHashMap<u64, u64, DefaultMemoryImpl> {
    let map = StableLinearHashMap::new(DefaultMemoryImpl::default()).expect("new linear map");
    for &(key, value) in entries {
        assert_eq!(map.insert(key, value), Ok(None));
    }
    assert_eq!(map.len(), Ok(entries.len() as u64));
    for &(key, value) in entries {
        assert_eq!(map.get(&key), Ok(Some(value)));
    }
    map
}

fn populated_btree(entries: &[(u64, u64)]) -> StableBTreeMap<u64, u64, DefaultMemoryImpl> {
    let mut map = StableBTreeMap::new(DefaultMemoryImpl::default());
    for &(key, value) in entries {
        assert_eq!(map.insert(key, value), None);
    }
    assert_eq!(map.len(), entries.len() as u64);
    for &(key, value) in entries {
        assert_eq!(map.get(&key), Some(value));
    }
    map
}

fn large_value(key: u64) -> [u8; 2048] {
    [key as u8; 2048]
}

fn populated_large() -> StableLinearHashMap<u64, [u8; 2048], DefaultMemoryImpl> {
    let map = StableLinearHashMap::new_with_hash_seed(DefaultMemoryImpl::default(), 317)
        .expect("new large-value map");
    for key in 0..LARGE_N {
        assert_eq!(map.insert(key, large_value(key)), Ok(None));
    }
    assert_eq!(map.len(), Ok(LARGE_N));
    for key in 0..LARGE_N {
        assert_eq!(map.get(&key), Ok(Some(large_value(key))));
    }
    map
}

#[cfg(target_family = "wasm")]
fn one_hop_movable_fixture<V: Storable + StableMapValue + Clone>(
    map: &StableLinearHashMap<u64, V, DefaultMemoryImpl>,
    value: impl Fn(u64) -> V,
) -> (u64, Vec<(u64, V)>, u64, u64) {
    let target = (1u64..)
        .find(|key| {
            let candidates = map.probe_one_hop_candidates(*key);
            candidates.0 != candidates.1
        })
        .expect("distinct target candidates");
    let candidates = map.probe_one_hop_candidates(target);
    let (moved, destination) = (1u64..)
        .find_map(|key| {
            let routes = map.probe_one_hop_candidates(key);
            let destination = if routes.0 == candidates.0 && routes.1 != candidates.0 {
                routes.1
            } else if routes.1 == candidates.0 && routes.0 != candidates.0 {
                routes.0
            } else {
                return None;
            };
            (key != target && destination != candidates.0 && destination != candidates.1)
                .then_some((key, destination))
        })
        .expect("movable resident");
    let mut occupancies = [0u8; 8];
    let mut used = vec![target];
    let mut residents = Vec::new();
    let place = |bucket: u64,
                 key: u64,
                 occupancies: &mut [u8; 8],
                 used: &mut Vec<u64>,
                 residents: &mut Vec<(u64, V)>| {
        let occupancy = occupancies[bucket as usize];
        let slot = (!occupancy).trailing_zeros();
        let resident_value = value(key);
        map.probe_one_hop_place(bucket, slot, occupancy, key, resident_value.clone());
        occupancies[bucket as usize] |= 1 << slot;
        used.push(key);
        residents.push((key, resident_value));
    };
    place(
        candidates.0,
        moved,
        &mut occupancies,
        &mut used,
        &mut residents,
    );
    for bucket in [candidates.0, candidates.1] {
        while occupancies[bucket as usize] != u8::MAX {
            let key = (1u64..)
                .find(|key| {
                    !used.contains(key) && {
                        let routes = map.probe_one_hop_candidates(*key);
                        routes.0 == bucket || routes.1 == bucket
                    }
                })
                .expect("candidate filler");
            place(bucket, key, &mut occupancies, &mut used, &mut residents);
        }
    }
    map.probe_one_hop_set_len(residents.len() as u64);
    (target, residents, moved, destination)
}

#[cfg(target_family = "wasm")]
fn one_hop_exhausted_fixture(
    map: &StableLinearHashMap<u64, u64, DefaultMemoryImpl>,
) -> (u64, Vec<(u64, u64)>) {
    let target = (1u64..)
        .find(|key| {
            let candidates = map.probe_one_hop_candidates(*key);
            candidates.0 != candidates.1
        })
        .expect("distinct exhausted target");
    let candidates = map.probe_one_hop_candidates(target);
    let keys = (target + 1..)
        .filter(|key| map.probe_one_hop_candidates(*key) == candidates)
        .take(16)
        .collect::<Vec<_>>();
    let mut occupancies = [0u8; 8];
    let mut residents = Vec::new();
    for key in keys {
        let bucket = if occupancies[candidates.0 as usize] != u8::MAX {
            candidates.0
        } else {
            candidates.1
        };
        let occupancy = occupancies[bucket as usize];
        let slot = (!occupancy).trailing_zeros();
        let value = fixture_value(key);
        map.probe_one_hop_place(bucket, slot, occupancy, key, value);
        occupancies[bucket as usize] |= 1 << slot;
        residents.push((key, value));
    }
    map.probe_one_hop_set_len(residents.len() as u64);
    (target, residents)
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_one_hop_movable() -> canbench_rs::BenchResult {
    let preflight = StableLinearHashMap::new_with_hash_seed(DefaultMemoryImpl::default(), 443)
        .expect("new preflight map");
    let (preflight_target, _, _, _) = one_hop_movable_fixture(&preflight, fixture_value);
    assert_eq!(
        preflight.insert(preflight_target, fixture_value(preflight_target)),
        Ok(None)
    );

    let map = StableLinearHashMap::new_with_hash_seed(DefaultMemoryImpl::default(), 443)
        .expect("new measured map");
    let (target, residents, moved, destination) = one_hop_movable_fixture(&map, fixture_value);
    let before = map.control_region().expect("pre-measurement control");
    let measured = Rc::new(RefCell::new(None));
    let measured_in_scope = measured.clone();
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_one_hop_movable");
        *measured_in_scope.borrow_mut() = Some(black_box(
            map.insert(black_box(target), black_box(fixture_value(target))),
        ));
    });
    assert_eq!(measured.borrow().as_ref(), Some(&Ok(None)));
    let after = map.control_region().expect("post-measurement control");
    assert_eq!(after.len, before.len + 1);
    assert_eq!(
        (after.level, after.split_cursor, after.physical_buckets),
        (before.level, before.split_cursor, before.physical_buckets)
    );
    assert_eq!(after.mutation_epoch, before.mutation_epoch + 2);
    assert_eq!(map.get(&target), Ok(Some(fixture_value(target))));
    assert_eq!(map.probe_one_hop_resident_bucket(moved), destination);
    for &(key, value) in &residents {
        assert_eq!(map.get(&key), Ok(Some(value)));
    }
    let reopened =
        StableLinearHashMap::<u64, u64, _>::init(map.into_memory()).expect("reopen movable map");
    assert_eq!(reopened.get(&target), Ok(Some(fixture_value(target))));
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_one_hop_exhausted() -> canbench_rs::BenchResult {
    let map = StableLinearHashMap::new_with_hash_seed(DefaultMemoryImpl::default(), 449)
        .expect("new exhausted map");
    let (target, residents) = one_hop_exhausted_fixture(&map);
    assert_eq!(
        map.insert(target, fixture_value(target)),
        Err(crate::MutationError::TablePressure)
    );
    let before = map.control_region().expect("pre-measurement control");
    let measured = Rc::new(RefCell::new(None));
    let measured_in_scope = measured.clone();
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_one_hop_exhausted");
        *measured_in_scope.borrow_mut() = Some(black_box(
            map.insert(black_box(target), black_box(fixture_value(target))),
        ));
    });
    assert_eq!(
        measured.borrow().as_ref(),
        Some(&Err(crate::MutationError::TablePressure))
    );
    assert_eq!(
        map.control_region().expect("post-measurement control"),
        before
    );
    assert_eq!(map.get(&target), Ok(None));
    for &(key, value) in &residents {
        assert_eq!(map.get(&key), Ok(Some(value)));
    }
    let reopened =
        StableLinearHashMap::<u64, u64, _>::init(map.into_memory()).expect("reopen exhausted map");
    assert_eq!(reopened.get(&target), Ok(None));
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_one_hop_movable_large_value() -> canbench_rs::BenchResult {
    let preflight = StableLinearHashMap::new_with_hash_seed(DefaultMemoryImpl::default(), 457)
        .expect("new large preflight map");
    let (preflight_target, _, _, _) = one_hop_movable_fixture(&preflight, large_value);
    assert_eq!(
        preflight.insert(preflight_target, large_value(preflight_target)),
        Ok(None)
    );

    let map = StableLinearHashMap::new_with_hash_seed(DefaultMemoryImpl::default(), 457)
        .expect("new large measured map");
    let (target, residents, moved, destination) = one_hop_movable_fixture(&map, large_value);
    let before = map.control_region().expect("pre-measurement control");
    let measured = Rc::new(RefCell::new(None));
    let measured_in_scope = measured.clone();
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_one_hop_movable_large_value");
        *measured_in_scope.borrow_mut() = Some(black_box(
            map.insert(black_box(target), black_box(large_value(target))),
        ));
    });
    assert_eq!(measured.borrow().as_ref(), Some(&Ok(None)));
    let after = map.control_region().expect("post-measurement control");
    assert_eq!(after.len, before.len + 1);
    assert_eq!(
        (after.level, after.split_cursor, after.physical_buckets),
        (before.level, before.split_cursor, before.physical_buckets)
    );
    assert_eq!(after.mutation_epoch, before.mutation_epoch + 2);
    assert_eq!(map.get(&target), Ok(Some(large_value(target))));
    assert_eq!(map.probe_one_hop_resident_bucket(moved), destination);
    for &(key, ref value) in &residents {
        assert_eq!(map.get(&key), Ok(Some(*value)));
    }
    let reopened = StableLinearHashMap::<u64, [u8; 2048], _>::init(map.into_memory())
        .expect("reopen large movable map");
    assert_eq!(reopened.get(&target), Ok(Some(large_value(target))));
    result
}

#[bench(raw)]
fn bench_linear_contains_miss_large_16() -> canbench_rs::BenchResult {
    let map = populated_large();
    let misses = [
        u64::MAX - 15,
        u64::MAX - 14,
        u64::MAX - 13,
        u64::MAX - 12,
        u64::MAX - 11,
        u64::MAX - 10,
        u64::MAX - 9,
        u64::MAX - 8,
        u64::MAX - 7,
        u64::MAX - 6,
        u64::MAX - 5,
        u64::MAX - 4,
        u64::MAX - 3,
        u64::MAX - 2,
        u64::MAX - 1,
        u64::MAX,
    ];
    for key in misses {
        assert_eq!(map.contains_key(&key), Ok(false));
    }
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_linear_contains_miss_large_16");
        for key in misses {
            let _ = black_box(map.contains_key(&black_box(key)));
        }
    });
    assert_eq!(map.len(), Ok(LARGE_N));
    let reopened = StableLinearHashMap::<u64, [u8; 2048], _>::init(map.into_memory())
        .expect("reopen large miss fixture");
    for key in 0..LARGE_N {
        assert_eq!(reopened.get(&key), Ok(Some(large_value(key))));
    }
    result
}

#[bench(raw)]
fn bench_linear_get_hit_large_16() -> canbench_rs::BenchResult {
    let map = populated_large();
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_linear_get_hit_large_16");
        for key in 0..LARGE_N {
            let _ = black_box(map.get(&black_box(key)));
        }
    });
    assert_eq!(map.len(), Ok(LARGE_N));
    for key in 0..LARGE_N {
        assert_eq!(map.get(&key), Ok(Some(large_value(key))));
    }
    let reopened = StableLinearHashMap::<u64, [u8; 2048], _>::init(map.into_memory())
        .expect("reopen large get fixture");
    for key in 0..LARGE_N {
        assert_eq!(reopened.get(&key), Ok(Some(large_value(key))));
    }
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_insert_split_move_4_large() -> canbench_rs::BenchResult {
    let (&target, residents) = SPLIT_MOVE_4.split_last().expect("large split fixture");
    let map = StableLinearHashMap::new_with_hash_seed(DefaultMemoryImpl::default(), SPLIT_SEED)
        .expect("new large split map");
    for &key in residents {
        assert_eq!(map.insert(key, large_value(key)), Ok(None));
    }
    let before = map.control_region().expect("pre-split control");
    assert_eq!(
        (
            before.level,
            before.split_cursor,
            before.physical_buckets,
            before.len
        ),
        (3, 0, 8, 48)
    );
    assert_eq!(map.get(&target), Ok(None));
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_linear_insert_split_move_4_large");
        black_box(map.insert(black_box(target), black_box(large_value(target))))
    });
    let after = map.control_region().expect("post-split control");
    assert_eq!(
        (
            after.level,
            after.split_cursor,
            after.physical_buckets,
            after.len
        ),
        (3, 1, 9, 49)
    );
    for &key in &SPLIT_MOVE_4 {
        assert_eq!(map.get(&key), Ok(Some(large_value(key))));
    }
    let reopened = StableLinearHashMap::<u64, [u8; 2048], _>::init(map.into_memory())
        .expect("reopen large split fixture");
    for &key in &SPLIT_MOVE_4 {
        assert_eq!(reopened.get(&key), Ok(Some(large_value(key))));
    }
    result
}

#[cfg(target_family = "wasm")]
fn split_benchmark(
    scope: &'static str,
    seed: u64,
    keys: &[u64],
    expected_before: (u8, u64, u64, u64),
    expected_after: (u8, u64, u64, u64),
    expected_moves: usize,
) -> canbench_rs::BenchResult {
    let (&target, residents) = keys.split_last().expect("split fixture has a target");
    let map = StableLinearHashMap::new_with_hash_seed(DefaultMemoryImpl::default(), seed)
        .expect("new split benchmark map");
    for &key in residents {
        assert_eq!(map.insert(key, fixture_value(key)), Ok(None));
    }
    let before = map.control_region().expect("idle pre-split control");
    assert_eq!(
        (
            before.level,
            before.split_cursor,
            before.physical_buckets,
            before.len
        ),
        expected_before
    );
    assert_eq!(map.get(&target), Ok(None));
    let source = before.split_cursor;
    let new_bucket = source + (1u64 << before.level);
    assert_eq!(map.probe_bucket_occupancy(source), u8::MAX);
    let source_keys = residents
        .iter()
        .copied()
        .filter(|key| map.probe_resident_bucket(*key) == source)
        .collect::<Vec<_>>();
    assert_eq!(source_keys.len(), 8);
    for &key in &source_keys {
        let routes = map.probe_candidates(key);
        assert!(routes.0 == source || routes.1 == source);
    }
    let target_routes = map.probe_candidates(target);
    assert!(target_routes.0 < before.physical_buckets);
    assert!(target_routes.1 < before.physical_buckets);
    for &key in residents {
        assert_eq!(map.get(&key), Ok(Some(fixture_value(key))));
    }

    let preflight = StableLinearHashMap::new_with_hash_seed(DefaultMemoryImpl::default(), seed)
        .expect("new equivalent preflight map");
    for &key in residents {
        assert_eq!(preflight.insert(key, fixture_value(key)), Ok(None));
    }
    assert_eq!(preflight.insert(target, fixture_value(target)), Ok(None));
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        black_box(map.insert(black_box(target), black_box(fixture_value(target))))
    });

    let after = map.control_region().expect("idle post-split control");
    assert_eq!(
        (
            after.level,
            after.split_cursor,
            after.physical_buckets,
            after.len
        ),
        expected_after
    );
    let target_bucket = map.probe_resident_bucket(target);
    assert_eq!(
        map.probe_bucket_occupancy(source).count_ones() as usize,
        8 - expected_moves + usize::from(target_bucket == source)
    );
    assert_eq!(
        map.probe_bucket_occupancy(new_bucket).count_ones() as usize,
        expected_moves + usize::from(target_bucket == new_bucket)
    );
    let moved = source_keys
        .iter()
        .filter(|key| map.probe_resident_bucket(**key) == new_bucket)
        .count();
    assert_eq!(moved, expected_moves);
    for &key in &source_keys {
        let routes = map.probe_candidates(key);
        let bucket = map.probe_resident_bucket(key);
        assert!(routes.0 == bucket || routes.1 == bucket);
    }
    let target_routes = map.probe_candidates(target);
    assert!(target_routes.0 == target_bucket || target_routes.1 == target_bucket);
    for &key in keys {
        assert_eq!(map.get(&key), Ok(Some(fixture_value(key))));
    }
    let reopened = StableLinearHashMap::<u64, u64, _>::init(map.into_memory())
        .expect("reopen split benchmark map");
    assert_eq!(reopened.control_region(), Ok(after));
    for &key in keys {
        assert_eq!(reopened.get(&key), Ok(Some(fixture_value(key))));
    }
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_insert_split_move_0() -> canbench_rs::BenchResult {
    split_benchmark(
        "bench_linear_insert_split_move_0",
        SPLIT_SEED,
        &SPLIT_MOVE_0,
        (3, 0, 8, 48),
        (3, 1, 9, 49),
        0,
    )
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_insert_split_move_4() -> canbench_rs::BenchResult {
    split_benchmark(
        "bench_linear_insert_split_move_4",
        SPLIT_SEED,
        &SPLIT_MOVE_4,
        (3, 0, 8, 48),
        (3, 1, 9, 49),
        4,
    )
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_insert_split_move_8() -> canbench_rs::BenchResult {
    split_benchmark(
        "bench_linear_insert_split_move_8",
        SPLIT_SEED,
        &SPLIT_MOVE_8,
        (3, 0, 8, 48),
        (3, 1, 9, 49),
        8,
    )
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_insert_split_round_rollover() -> canbench_rs::BenchResult {
    split_benchmark(
        "bench_linear_insert_split_round_rollover",
        233,
        &SPLIT_ROLLOVER,
        (3, 7, 15, 90),
        (4, 0, 16, 91),
        4,
    )
}

#[bench(raw)]
fn bench_linear_get_48() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let map = populated_linear(&entries);
    for &(key, value) in &entries {
        assert_eq!(map.get(&key), Ok(Some(value)));
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_linear_get_48");
        for &(key, _) in &entries {
            let _ = black_box(map.get(&black_box(key)));
        }
    })
}

#[bench(raw)]
fn bench_linear_scan_physical_slots_64() -> canbench_rs::BenchResult {
    let map = StableLinearHashMap::new(DefaultMemoryImpl::default()).expect("new scan map");
    let mut entries = Vec::with_capacity(N);
    for candidate in 0u64.. {
        let value = fixture_value(candidate);
        match map.insert(candidate, value) {
            Ok(None) => {
                entries.push((candidate, value));
                if entries.len() == N {
                    break;
                }
            }
            Ok(Some(_)) => panic!("scan fixture candidates must be unique"),
            Err(_) => {}
        }
    }
    assert_eq!(entries.len(), N);
    let cursor = map.scan_start().expect("scan cursor");
    let preflight = map.scan_step(cursor, 64).expect("preflight scan");
    assert_eq!(preflight.entries().len(), entries.len());
    assert_eq!(preflight.examined_slots(), 64);
    assert!(preflight.exhausted());
    let mut expected = entries.clone();
    expected.sort_unstable();
    let mut actual = preflight.entries().to_vec();
    actual.sort_unstable();
    assert_eq!(actual, expected);

    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_linear_scan_physical_slots_64");
        let _ = black_box(map.scan_step(black_box(cursor), black_box(64)));
    });

    let verified = map.scan_step(cursor, 64).expect("verified scan");
    assert_eq!(verified.entries().len(), entries.len());
    assert_eq!(verified.examined_slots(), 64);
    assert!(verified.exhausted());
    let mut actual = verified.into_entries();
    actual.sort_unstable();
    assert_eq!(actual, expected);
    result
}

#[bench(raw)]
fn bench_linear_insert_48() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let preflight = populated_linear(&entries);
    assert_eq!(preflight.len(), Ok(entries.len() as u64));
    let map = StableLinearHashMap::new(DefaultMemoryImpl::default()).expect("new linear map");
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_linear_insert_48");
        for &(key, value) in &entries {
            let _ = black_box(map.insert(key, value));
        }
    });
    assert_eq!(map.len(), Ok(entries.len() as u64));
    for (key, value) in entries {
        assert_eq!(map.get(&key), Ok(Some(value)));
    }
    result
}

#[bench(raw)]
fn bench_linear_remove_48() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let preflight = populated_linear(&entries);
    for &(key, value) in &entries {
        assert_eq!(preflight.remove(&key), Ok(Some(value)));
    }
    assert_eq!(preflight.is_empty(), Ok(true));
    assert_eq!(preflight.len(), Ok(0));
    for &(key, _) in &entries {
        assert_eq!(preflight.get(&key), Ok(None));
    }
    let map = populated_linear(&entries);
    assert_eq!(map.len(), Ok(entries.len() as u64));
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_linear_remove_48");
        for &(key, _) in &entries {
            let _ = black_box(map.remove(&key));
        }
    });
    assert_eq!(map.is_empty(), Ok(true));
    assert_eq!(map.len(), Ok(0));
    for (key, _) in entries {
        assert_eq!(map.get(&key), Ok(None));
    }
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_get_phases_48() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let map = populated_linear(&entries);
    let seeds: Vec<_> = entries.iter().map(|_| map.probe_seed()).collect();
    let encoded_keys: Vec<_> = entries
        .iter()
        .map(|&(key, _)| map.probe_key_encode(key))
        .collect();
    let prepared_routes: Vec<_> = encoded_keys
        .iter()
        .zip(&seeds)
        .map(|(&bytes, &seed)| map.prepare_route(bytes, seed))
        .collect();
    let first_hashes: Vec<_> = prepared_routes
        .iter()
        .map(|route| map.probe_first_hash(route))
        .collect();
    let second_hashes: Vec<_> = prepared_routes
        .iter()
        .map(|route| map.probe_second_hash(route))
        .collect();
    let routes: Vec<_> = entries
        .iter()
        .zip(first_hashes.iter().zip(&second_hashes))
        .map(|(_entry, (&first_hash, &second_hash))| {
            map.probe_bucket_mapping(first_hash, second_hash)
        })
        .collect();
    let result = canbench_rs::bench_fn(|| {
        {
            let _scope = canbench_rs::bench_scope("get_seed");
            for _ in &entries {
                black_box(map.probe_seed());
            }
        }
        {
            let _scope = canbench_rs::bench_scope("get_route_hash");
            for (&(key, _), &seed) in entries.iter().zip(&seeds) {
                black_box(map.probe_route_hash(black_box(key), black_box(seed)));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("get_route_key_encode");
            for &(key, _) in &entries {
                black_box(map.probe_key_encode(black_box(key)));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("get_route_secret_cache_hit");
            for &seed in &seeds {
                black_box(map.probe_secret_cache_hit(black_box(seed)));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("get_route_first_hash");
            for route in &prepared_routes {
                black_box(map.probe_first_hash(black_box(route)));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("get_route_second_hash");
            for route in &prepared_routes {
                black_box(map.probe_second_hash(black_box(route)));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("get_route_bucket_mapping");
            for (&first_hash, &second_hash) in first_hashes.iter().zip(&second_hashes) {
                black_box(map.probe_bucket_mapping(black_box(first_hash), black_box(second_hash)));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("get_bucket_value");
            for (&(key, _), &route) in entries.iter().zip(&routes) {
                black_box(map.probe_bucket_value(black_box(key), black_box(route)));
            }
        }
    });
    assert_eq!(map.len(), Ok(entries.len() as u64));
    for ((&(key, value), &seed), (&first_hash, &second_hash)) in entries
        .iter()
        .zip(&seeds)
        .zip(first_hashes.iter().zip(&second_hashes))
    {
        assert_eq!(map.get(&key), Ok(Some(value)));
        assert_eq!(
            map.probe_bucket_mapping(first_hash, second_hash),
            map.probe_route_hash(key, seed)
        );
    }
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_insert_phases_48() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let route_maps: Vec<_> = (0..N)
        .map(|id| {
            StableLinearHashMap::new(manager.get(MemoryId::new(id as u8))).expect("route fixture")
        })
        .collect();
    let payload_maps: Vec<_> = (0..N)
        .map(|id| {
            StableLinearHashMap::new(manager.get(MemoryId::new((N + id) as u8)))
                .expect("payload fixture")
        })
        .collect();
    let metadata_maps: Vec<_> = (0..N)
        .map(|id| {
            StableLinearHashMap::new(manager.get(MemoryId::new((2 * N + id) as u8)))
                .expect("metadata fixture")
        })
        .collect();
    let payload_routes: Vec<_> = payload_maps
        .iter()
        .zip(&entries)
        .map(|(map, &(key, _))| map.probe_insert_control_route_lookup(key))
        .collect();
    let metadata_routes: Vec<_> = metadata_maps
        .iter()
        .zip(&entries)
        .map(|(map, &(key, value))| {
            let route = map.probe_insert_control_route_lookup(key);
            map.probe_insert_payload_write(key, value, route);
            route
        })
        .collect();
    let route_routes: Vec<_> = route_maps
        .iter()
        .zip(&entries)
        .map(|(map, &(key, _))| map.probe_insert_control_route_lookup(key))
        .collect();
    let result = canbench_rs::bench_fn(|| {
        {
            let _scope = canbench_rs::bench_scope("insert_control_route_lookup");
            for (map, &(key, _)) in route_maps.iter().zip(&entries) {
                black_box(map.probe_insert_control_route_lookup(black_box(key)));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("insert_payload_write");
            for ((map, &(key, value)), &route) in
                payload_maps.iter().zip(&entries).zip(&payload_routes)
            {
                map.probe_insert_payload_write(black_box(key), black_box(value), route);
            }
        }
        {
            let _scope = canbench_rs::bench_scope("insert_metadata_publish");
            for (map, &route) in metadata_maps.iter().zip(&metadata_routes) {
                map.probe_insert_metadata_publish(route);
            }
        }
    });
    for ((map, &(key, _)), &route) in route_maps.iter().zip(&entries).zip(&route_routes) {
        assert_eq!(map.len(), Ok(0));
        assert_eq!(map.get(&key), Ok(None));
        assert_eq!(map.probe_insert_control_route_lookup(key), route);
    }
    for ((map, &(key, value)), &route) in payload_maps.iter().zip(&entries).zip(&payload_routes) {
        assert_eq!(map.len(), Ok(0));
        assert_eq!(map.get(&key), Ok(None));
        assert_eq!(map.probe_insert_control_route_lookup(key), route);
        assert!(map.probe_payload_equals(key, value, route));
    }
    for (map, (&(key, value), &_route)) in metadata_maps
        .into_iter()
        .zip(entries.iter().zip(&metadata_routes))
    {
        assert_eq!(map.len(), Ok(1));
        assert_eq!(map.get(&key), Ok(Some(value)));
        let reopened = StableLinearHashMap::<u64, u64, _>::init(map.into_memory())
            .expect("reopen metadata fixture");
        assert_eq!(reopened.len(), Ok(1));
        assert_eq!(reopened.get(&key), Ok(Some(value)));
    }
    result
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_linear_remove_phases_48() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let make_maps = |offset: usize| {
        entries
            .iter()
            .enumerate()
            .map(|(id, &(key, value))| {
                let map = StableLinearHashMap::new(manager.get(MemoryId::new((offset + id) as u8)))
                    .expect("remove fixture");
                assert_eq!(map.insert(key, value), Ok(None));
                map
            })
            .collect::<Vec<_>>()
    };
    let route_maps = make_maps(0);
    let metadata_maps = make_maps(N);
    let route_routes: Vec<_> = route_maps
        .iter()
        .zip(&entries)
        .map(|(map, &(key, _))| map.probe_remove_control_route_bucket_value(key))
        .collect();
    let metadata_routes: Vec<_> = metadata_maps
        .iter()
        .zip(&entries)
        .map(|(map, &(key, _))| map.probe_remove_control_route_bucket_value(key))
        .collect();
    for ((map, &(key, value)), &route) in metadata_maps.iter().zip(&entries).zip(&metadata_routes) {
        assert_eq!(route.1, value);
        assert_eq!(map.get(&key), Ok(Some(value)));
    }
    let result = canbench_rs::bench_fn(|| {
        {
            let _scope = canbench_rs::bench_scope("remove_control_route_bucket_value");
            for (map, &(key, _)) in route_maps.iter().zip(&entries) {
                black_box(map.probe_remove_control_route_bucket_value(black_box(key)));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("remove_metadata_publish");
            for (map, &route) in metadata_maps.iter().zip(&metadata_routes) {
                map.probe_remove_metadata_publish(route.0);
            }
        }
    });
    for ((map, &(key, value)), &route) in route_maps.iter().zip(&entries).zip(&route_routes) {
        assert_eq!(route.1, value);
        assert_eq!(map.probe_remove_control_route_bucket_value(key), route);
        assert_eq!(map.len(), Ok(1));
        assert_eq!(map.get(&key), Ok(Some(value)));
    }
    for (map, (&(key, _), _)) in metadata_maps
        .into_iter()
        .zip(entries.iter().zip(&metadata_routes))
    {
        assert_eq!(map.len(), Ok(0));
        assert_eq!(map.get(&key), Ok(None));
        let reopened = StableLinearHashMap::<u64, u64, _>::init(map.into_memory())
            .expect("reopen remove metadata fixture");
        assert_eq!(reopened.len(), Ok(0));
        assert_eq!(reopened.get(&key), Ok(None));
    }
    result
}

#[bench(raw)]
fn bench_btree_get_48() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let map = populated_btree(&entries);
    for &(key, value) in &entries {
        assert_eq!(map.get(&key), Some(value));
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_btree_get_48");
        for &(key, _) in &entries {
            black_box(map.get(&black_box(key)));
        }
    })
}

#[bench(raw)]
fn bench_btree_insert_48() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let preflight = populated_btree(&entries);
    assert_eq!(preflight.len(), entries.len() as u64);
    let mut map = StableBTreeMap::new(DefaultMemoryImpl::default());
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_btree_insert_48");
        for &(key, value) in &entries {
            let _ = black_box(map.insert(key, value));
        }
    });
    assert_eq!(map.len(), entries.len() as u64);
    for (key, value) in entries {
        assert_eq!(map.get(&key), Some(value));
    }
    result
}

#[bench(raw)]
fn bench_btree_remove_48() -> canbench_rs::BenchResult {
    let entries = fixture_entries();
    let mut preflight = populated_btree(&entries);
    for &(key, value) in &entries {
        assert_eq!(preflight.remove(&key), Some(value));
    }
    assert!(preflight.is_empty());
    assert_eq!(preflight.len(), 0);
    for &(key, _) in &entries {
        assert_eq!(preflight.get(&key), None);
    }
    let mut map = populated_btree(&entries);
    assert_eq!(map.len(), entries.len() as u64);
    let result = canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_btree_remove_48");
        for &(key, _) in &entries {
            let _ = black_box(map.remove(&key));
        }
    });
    assert!(map.is_empty());
    assert_eq!(map.len(), 0);
    for (key, _) in entries {
        assert_eq!(map.get(&key), None);
    }
    result
}
