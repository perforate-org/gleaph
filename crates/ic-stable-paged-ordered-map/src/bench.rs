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
