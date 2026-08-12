use crate::StableLinearHashMap;
use canbench_rs::bench;
#[cfg(target_family = "wasm")]
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager};
use ic_stable_structures::{DefaultMemoryImpl, StableBTreeMap};
use std::hint::black_box;

const N: usize = 48;

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
