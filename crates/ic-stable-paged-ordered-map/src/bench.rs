use std::{
    borrow::Cow,
    hint::black_box,
    ops::Bound::{Excluded, Unbounded},
};

use canbench_rs::bench;
use ic_stable_structures::{DefaultMemoryImpl, StableBTreeMap, Storable, storable::Bound};

use crate::StablePagedOrderedMap;

const RNG_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
const MIXED_OPS: u64 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key(u64);

impl Storable for Key {
    const BOUND: Bound = u64::BOUND;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        self.0.to_bytes()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(u64::from_bytes(bytes))
    }
}

#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn random_key(i: u64, seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(splitmix64(*seed));
    i.saturating_mul(1_000_000).saturating_add(*seed % 997)
}

fn populate_paged(n: u64) -> StablePagedOrderedMap<DefaultMemoryImpl> {
    let map = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).expect("paged map");
    let mut seed = RNG_SEED;
    for i in 0..n {
        let key = random_key(i, &mut seed);
        map.insert(key, key ^ 0xA5A5).expect("insert paged");
    }
    map
}

fn populate_btree(n: u64) -> StableBTreeMap<Key, u64, DefaultMemoryImpl> {
    let mut map = StableBTreeMap::init(DefaultMemoryImpl::default());
    let mut seed = RNG_SEED;
    for i in 0..n {
        let key = random_key(i, &mut seed);
        map.insert(Key(key), key ^ 0xA5A5);
    }
    map
}

fn bench_paged_predecessor_successor(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let map = populate_paged(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let key = black_box(n / 2 * 1_000_000 + 500);
        let prev = map.predecessor(key).expect("predecessor");
        let next = map.successor(key).expect("successor");
        black_box(prev.0 ^ next.0);
    })
}

fn bench_btree_predecessor_successor(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let map = populate_btree(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let key = Key(black_box(n / 2 * 1_000_000 + 500));
        let prev = map
            .range((Unbounded, Excluded(key)))
            .next_back()
            .expect("predecessor");
        let next = map
            .range((Excluded(key), Unbounded))
            .next()
            .expect("successor");
        black_box(prev.key().0 ^ next.key().0);
    })
}

fn bench_paged_mixed_insert_remove(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let map = populate_paged(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        for i in 0..MIXED_OPS {
            let key = black_box((n + i).saturating_mul(1_000_000));
            map.insert(key, key + 1).expect("insert paged");
            let removed = map.remove(key).expect("remove paged").expect("present");
            black_box(removed);
        }
    })
}

fn bench_btree_mixed_insert_remove(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let mut map = populate_btree(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        for i in 0..MIXED_OPS {
            let key = Key(black_box((n + i).saturating_mul(1_000_000)));
            map.insert(key, key.0 + 1);
            let removed = map.remove(&key).expect("present");
            black_box(removed);
        }
    })
}

#[bench(raw)]
fn bench_paged_scalar_edits_97() -> canbench_rs::BenchResult {
    let map = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).expect("paged map");
    crate::seed_scalar_work(&map);
    let value = |key| 0xa500_0000_0000_0000 | key;
    let changed = value(64) ^ (1 << 56);
    assert_eq!(map.insert(64, changed), Ok(Some(value(64))));
    assert_eq!(map.get(64), Some(changed));
    assert_eq!(map.insert(64, value(64)), Ok(Some(changed)));
    assert_eq!(map.remove(129), Ok(None));
    assert_eq!(map.insert(2000, value(2000)), Ok(None));
    assert_eq!(map.remove(2000), Ok(Some(value(2000))));
    let expected: Vec<_> = (0..=126)
        .step_by(2)
        .chain((128..=252).step_by(4))
        .chain([1000])
        .map(|key| (key, value(key)))
        .collect();
    assert_eq!(map.iter().collect::<Vec<_>>(), expected);
    assert_eq!(map.header().page_count, 2);
    assert_eq!(map.header().dir_len, 2);
    assert_eq!(map.read_page_header(1).len, 64);
    assert_eq!(map.read_page_header(2).len, 33);
    assert_eq!(map.read_dir_entry(1).min_key, 128);
    map.validate().unwrap();
    let result = canbench_rs::bench_fn(|| {
        {
            let _scope = canbench_rs::bench_scope("scalar_replace_64");
            for i in 0..64 {
                let next = if i % 2 == 0 { changed } else { value(64) };
                black_box(map.insert(black_box(64), black_box(next)).expect("replace"));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("scalar_missing_64");
            for _ in 0..64 {
                black_box(map.remove(black_box(129)).expect("missing"));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("scalar_tail_pairs_64");
            for _ in 0..64 {
                black_box(map.insert(black_box(2000), value(2000)).expect("insert"));
                black_box(map.remove(black_box(2000)).expect("remove"));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("scalar_split_directory");
            black_box(map.insert(black_box(1), value(1)).expect("split"));
        }
    });
    let mut final_pairs = expected;
    final_pairs.insert(1, (1, value(1)));
    assert_eq!(map.iter().collect::<Vec<_>>(), final_pairs);
    assert_eq!(map.header().len, 98);
    assert_eq!(map.header().page_count, 3);
    assert_eq!(map.header().dir_len, 3);
    assert_eq!(map.read_page_header(1).len, 32);
    assert_eq!(map.read_page_header(3).len, 33);
    assert_eq!(map.read_page_header(1).next, 3);
    assert_eq!(map.read_page_header(3).next, 2);
    assert_eq!(map.read_page_header(2).prev, 3);
    assert_eq!(map.read_dir_entry(1).min_key, 62);
    assert_eq!(map.read_dir_entry(1).page_id, 3);
    assert_eq!(map.read_dir_entry(2).min_key, 128);
    assert_eq!(map.read_dir_entry(2).page_id, 2);
    map.validate().unwrap();
    result
}

#[bench(raw)]
fn bench_paged_predecessor_successor_1024() -> canbench_rs::BenchResult {
    bench_paged_predecessor_successor(1024, "paged_predecessor_successor")
}

#[bench(raw)]
fn bench_paged_predecessor_successor_4096() -> canbench_rs::BenchResult {
    bench_paged_predecessor_successor(4096, "paged_predecessor_successor")
}

#[bench(raw)]
fn bench_btree_predecessor_successor_1024() -> canbench_rs::BenchResult {
    bench_btree_predecessor_successor(1024, "btree_predecessor_successor")
}

#[bench(raw)]
fn bench_btree_predecessor_successor_4096() -> canbench_rs::BenchResult {
    bench_btree_predecessor_successor(4096, "btree_predecessor_successor")
}

#[bench(raw)]
fn bench_paged_mixed_insert_remove_1024() -> canbench_rs::BenchResult {
    bench_paged_mixed_insert_remove(1024, "paged_mixed_insert_remove")
}

#[bench(raw)]
fn bench_paged_mixed_insert_remove_4096() -> canbench_rs::BenchResult {
    bench_paged_mixed_insert_remove(4096, "paged_mixed_insert_remove")
}

#[bench(raw)]
fn bench_btree_mixed_insert_remove_1024() -> canbench_rs::BenchResult {
    bench_btree_mixed_insert_remove(1024, "btree_mixed_insert_remove")
}

#[bench(raw)]
fn bench_btree_mixed_insert_remove_4096() -> canbench_rs::BenchResult {
    bench_btree_mixed_insert_remove(4096, "btree_mixed_insert_remove")
}

#[bench(raw)]
fn bench_paged_missing_grouping_97() -> canbench_rs::BenchResult {
    use ic_stable_structures::Memory;

    let mut map = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).expect("paged map");
    crate::seed_scalar_work(&map);
    let expected: Vec<_> = (0_u64..=126)
        .step_by(2)
        .chain((128..=252).step_by(4))
        .chain([1000])
        .map(|key| (key, 0xa500_0000_0000_0000 | key))
        .collect();
    assert_eq!(map.iter().collect::<Vec<_>>(), expected);
    assert_eq!(map.header().len, 97);
    assert_eq!(map.header().page_count, 2);
    assert_eq!(map.header().dir_len, 2);
    assert_eq!(map.read_page_header(1).len, 64);
    assert_eq!(map.read_page_header(2).len, 33);
    assert_eq!(map.read_dir_entry(1).min_key, 128);
    assert_eq!(map.get(129), None);
    map.validate().unwrap();

    let backing_pages = map.memory.size();
    let mut before = vec![0; (backing_pages * crate::WASM_PAGE_SIZE) as usize];
    map.memory.read(0, &mut before);
    let mut after = vec![0; before.len()];
    let edits = [crate::Edit::Remove { key: 129 }; 64];
    let mut scalar_previous = [Some(u64::MAX); 64];
    let mut batch_previous = [Some(u64::MAX); 64];

    // Exercise both complete paths before measuring, including publication.
    for previous in &mut scalar_previous {
        *previous = black_box(map.remove(black_box(129)).expect("missing"));
    }
    assert_eq!(scalar_previous, [None; 64]);
    assert_eq!(map.memory.size(), backing_pages);
    map.memory.read(0, &mut after);
    assert_eq!(after, before);

    let ready = map
        .prepare_edits(black_box(&edits))
        .expect("prepare missing");
    assert_eq!(ready.previous_values().len(), 64);
    for (output, &previous) in batch_previous.iter_mut().zip(ready.previous_values()) {
        *output = black_box(previous);
    }
    ready.publish();
    assert_eq!(batch_previous, [None; 64]);
    assert_eq!(map.memory.size(), backing_pages);
    map.memory.read(0, &mut after);
    assert_eq!(after, before);
    scalar_previous.fill(Some(u64::MAX));
    batch_previous.fill(Some(u64::MAX));

    // Net grouping includes the batch's internal prior storage and its destruction.
    // Scalar keys cross a barrier per call; the batch input slice crosses one barrier.
    let result = canbench_rs::bench_fn(|| {
        {
            let _scope = canbench_rs::bench_scope("scalar_calls_64");
            for previous in &mut scalar_previous {
                *previous = black_box(map.remove(black_box(129)).expect("missing"));
            }
        }
        {
            let _scope = canbench_rs::bench_scope("prepared_batch_64");
            let ready = map
                .prepare_edits(black_box(&edits))
                .expect("prepare missing");
            for (output, &previous) in batch_previous.iter_mut().zip(ready.previous_values()) {
                *output = black_box(previous);
            }
            ready.publish();
        }
    });
    assert_eq!(scalar_previous, [None; 64]);
    assert_eq!(batch_previous, [None; 64]);
    assert_eq!(map.memory.size(), backing_pages);
    map.memory.read(0, &mut after);
    assert_eq!(after, before);
    assert_eq!(map.iter().collect::<Vec<_>>(), expected);
    map.validate().unwrap();
    result
}
