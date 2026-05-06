//! `canbench` workloads for LARA free span stores.
//!
//! This module is compiled only with the crate's `canbench` feature. It keeps the
//! measured workloads next to the free span implementations so benchmark coverage
//! can evolve with allocation, coalescing, and local gap-sliding behavior.

use std::{
    borrow::Cow,
    hint::black_box,
    ops::Bound::{Included, Unbounded},
};

use canbench_rs::bench;
use ic_stable_structures::{
    DefaultMemoryImpl, StableBTreeMap, Storable,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
    storable::Bound as StorableBound,
};

use super::{
    FreeSpan, FreeSpanArrayStore, FreeSpanBinnedBTreeStore, FreeSpanBinnedStore,
    FreeSpanDualIndexStore, FreeSpanStore,
};

const MIN_LEN: u64 = 96;
const COALESCE_LEN: u64 = 64;
const COALESCE_START: u64 = 9_000_000_000;
const MIXED_OPS: u64 = 16;
const RNG_SEED: u64 = 0x243F_6A88_85A3_08D3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PackedLenStart(u128);

impl PackedLenStart {
    #[inline]
    fn pack(len: u64, start: u64) -> Self {
        Self((u128::from(len) << 64) | u128::from(start))
    }

    #[inline]
    fn unpack(self) -> (u64, u64) {
        let len = (self.0 >> 64) as u64;
        let start = (self.0 & u128::from(u64::MAX)) as u64;
        (len, start)
    }
}

impl Storable for PackedLenStart {
    const BOUND: StorableBound = u128::BOUND;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        self.0.to_bytes()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(u128::from_bytes(bytes))
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

fn random_span(i: u64, seed: &mut u64) -> FreeSpan {
    *seed = seed.wrapping_add(splitmix64(*seed));
    FreeSpan {
        start_slot: i
            .saturating_mul(1_000_000)
            .saturating_add((*seed >> 32) % 1000),
        len: MIN_LEN + (*seed % 4096),
    }
}

fn populate_array_store(n: u64) -> FreeSpanArrayStore<DefaultMemoryImpl> {
    let store = FreeSpanArrayStore::new(DefaultMemoryImpl::default()).expect("free span store");
    let mut seed = RNG_SEED;
    for i in 0..n {
        store.push(random_span(i, &mut seed)).expect("push span");
    }
    store
}

fn populate_btree_map(n: u64) -> StableBTreeMap<PackedLenStart, (), DefaultMemoryImpl> {
    let mut map = StableBTreeMap::init(DefaultMemoryImpl::default());
    let mut seed = RNG_SEED;
    for i in 0..n {
        let span = random_span(i, &mut seed);
        assert!(
            map.insert(PackedLenStart::pack(span.len, span.start_slot), ())
                .is_none()
        );
    }
    map
}

fn populate_current_store(n: u64) -> FreeSpanStore<DefaultMemoryImpl> {
    let store = FreeSpanStore::init(DefaultMemoryImpl::default());
    let mut seed = RNG_SEED;
    for i in 0..n {
        store.release(random_span(i, &mut seed));
    }
    store
}

fn populate_dual_index_store(
    n: u64,
) -> FreeSpanDualIndexStore<VirtualMemory<DefaultMemoryImpl>, VirtualMemory<DefaultMemoryImpl>> {
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let by_len = manager.get(MemoryId::new(0));
    let by_start = manager.get(MemoryId::new(1));
    let mut store = FreeSpanDualIndexStore::init(by_len, by_start);
    let mut seed = RNG_SEED;
    for i in 0..n {
        let span = random_span(i, &mut seed);
        store.release(span).expect("insert dual-index span");
    }
    store
}

fn populate_dual_index_store_for_coalesce(
    n: u64,
) -> FreeSpanDualIndexStore<VirtualMemory<DefaultMemoryImpl>, VirtualMemory<DefaultMemoryImpl>> {
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let by_len = manager.get(MemoryId::new(0));
    let by_start = manager.get(MemoryId::new(1));
    let mut store = FreeSpanDualIndexStore::init(by_len, by_start);
    store
        .release_span(COALESCE_START, COALESCE_LEN)
        .expect("release previous neighbor");
    store
        .release_span(COALESCE_START + COALESCE_LEN * 2, COALESCE_LEN)
        .expect("release next neighbor");

    let mut seed = RNG_SEED;
    for i in 2..n {
        let mut span = random_span(i, &mut seed);
        span.start_slot = span.start_slot.saturating_add(100_000_000_000);
        store.release(span).expect("release distractor span");
    }
    store
}

fn populate_array_store_for_coalesce(n: u64) -> FreeSpanArrayStore<DefaultMemoryImpl> {
    let store = FreeSpanArrayStore::new(DefaultMemoryImpl::default()).expect("free span store");
    store
        .push(FreeSpan {
            start_slot: COALESCE_START,
            len: COALESCE_LEN,
        })
        .expect("push previous neighbor");
    store
        .push(FreeSpan {
            start_slot: COALESCE_START + COALESCE_LEN * 2,
            len: COALESCE_LEN,
        })
        .expect("push next neighbor");

    let mut seed = RNG_SEED;
    for i in 2..n {
        let mut span = random_span(i, &mut seed);
        span.start_slot = span.start_slot.saturating_add(100_000_000_000);
        store.push(span).expect("push distractor span");
    }
    store
}

fn populate_current_store_for_coalesce(n: u64) -> FreeSpanStore<DefaultMemoryImpl> {
    let store = FreeSpanStore::init(DefaultMemoryImpl::default());
    store.release(FreeSpan {
        start_slot: COALESCE_START,
        len: COALESCE_LEN,
    });
    store.release(FreeSpan {
        start_slot: COALESCE_START + COALESCE_LEN * 2,
        len: COALESCE_LEN,
    });

    let mut seed = RNG_SEED;
    for i in 2..n {
        let mut span = random_span(i, &mut seed);
        span.start_slot = span.start_slot.saturating_add(100_000_000_000);
        store.release(span);
    }
    store
}

fn populate_binned_store(
    n: u64,
) -> FreeSpanBinnedStore<VirtualMemory<DefaultMemoryImpl>, VirtualMemory<DefaultMemoryImpl>> {
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let store_memory = manager.get(MemoryId::new(20));
    let by_start = manager.get(MemoryId::new(21));
    let store = FreeSpanBinnedStore::init(store_memory, by_start).expect("binned store");
    let mut seed = RNG_SEED;
    for i in 0..n {
        let span = random_span(i, &mut seed);
        store.release(span).expect("release binned span");
    }
    store
}

fn populate_binned_store_for_coalesce(
    n: u64,
) -> FreeSpanBinnedStore<VirtualMemory<DefaultMemoryImpl>, VirtualMemory<DefaultMemoryImpl>> {
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let store_memory = manager.get(MemoryId::new(20));
    let by_start = manager.get(MemoryId::new(21));
    let store = FreeSpanBinnedStore::init(store_memory, by_start).expect("binned store");
    store
        .release_span(COALESCE_START, COALESCE_LEN)
        .expect("release previous neighbor");
    store
        .release_span(COALESCE_START + COALESCE_LEN * 2, COALESCE_LEN)
        .expect("release next neighbor");

    let mut seed = RNG_SEED;
    for i in 2..n {
        let mut span = random_span(i, &mut seed);
        span.start_slot = span.start_slot.saturating_add(100_000_000_000);
        store.release(span).expect("release distractor span");
    }
    store
}

fn populate_binned_btree_store(
    n: u64,
) -> FreeSpanBinnedBTreeStore<VirtualMemory<DefaultMemoryImpl>, VirtualMemory<DefaultMemoryImpl>> {
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let store_memory = manager.get(MemoryId::new(30));
    let by_start = manager.get(MemoryId::new(31));
    let store = FreeSpanBinnedBTreeStore::init(store_memory, by_start).expect("btree binned store");
    let mut seed = RNG_SEED;
    for i in 0..n {
        let span = random_span(i, &mut seed);
        store.release(span).expect("release btree binned span");
    }
    store
}

fn populate_binned_btree_store_for_coalesce(
    n: u64,
) -> FreeSpanBinnedBTreeStore<VirtualMemory<DefaultMemoryImpl>, VirtualMemory<DefaultMemoryImpl>> {
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let store_memory = manager.get(MemoryId::new(30));
    let by_start = manager.get(MemoryId::new(31));
    let store = FreeSpanBinnedBTreeStore::init(store_memory, by_start).expect("btree binned store");
    store
        .release_span(COALESCE_START, COALESCE_LEN)
        .expect("release previous neighbor");
    store
        .release_span(COALESCE_START + COALESCE_LEN * 2, COALESCE_LEN)
        .expect("release next neighbor");

    let mut seed = RNG_SEED;
    for i in 2..n {
        let mut span = random_span(i, &mut seed);
        span.start_slot = span.start_slot.saturating_add(100_000_000_000);
        store
            .release(span)
            .expect("release btree binned distractor span");
    }
    store
}

fn bench_array_take_best_fit(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_array_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let span = store
            .take_best_fit(black_box(MIN_LEN))
            .expect("take_best_fit")
            .expect("eligible span");
        store.push(span).expect("restore span");
        black_box(span.start_slot);
    })
}

fn bench_btree_take_best_fit(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let mut map = populate_btree_map(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let lower = PackedLenStart::pack(black_box(MIN_LEN), 0);
        let key = {
            let entry = map
                .range((Included(lower), Unbounded))
                .next()
                .expect("range hit");
            *entry.key()
        };
        map.remove(&key);
        assert!(map.insert(key, ()).is_none());
        black_box(key.unpack().1);
    })
}

fn bench_array_release_coalescing(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_array_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        store
            .release_coalescing_linear(FreeSpan {
                start_slot: COALESCE_START + COALESCE_LEN,
                len: COALESCE_LEN,
            })
            .expect("release coalescing");
        black_box(store.len());
    })
}

fn bench_current_take_best_fit_whole(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_current_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let span = store
            .take_best_fit_whole(black_box(MIN_LEN))
            .expect("eligible span");
        store.release(span);
        black_box(span.start_slot);
    })
}

fn bench_current_take_best_fit_split(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_current_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let span = store
            .take_best_fit(black_box(MIN_LEN))
            .expect("eligible span");
        store.release(span);
        black_box(span.start_slot);
    })
}

fn bench_current_neighbor_lookup(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_current_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let left = store
            .free_span_ending_at(black_box(COALESCE_START + COALESCE_LEN))
            .expect("left neighbor");
        let right = store
            .free_span_starting_at(black_box(COALESCE_START + COALESCE_LEN * 2))
            .expect("right neighbor");
        black_box(left.start_slot ^ right.start_slot);
    })
}

fn bench_current_release_coalescing(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_current_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        store.release(FreeSpan {
            start_slot: COALESCE_START + COALESCE_LEN,
            len: COALESCE_LEN,
        });
        black_box(store.len());
    })
}

fn bench_current_replace_exact_pair(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_current_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let left = store
            .free_span_ending_at(black_box(COALESCE_START + COALESCE_LEN))
            .expect("left neighbor");
        let right = store
            .free_span_starting_at(black_box(COALESCE_START + COALESCE_LEN * 2))
            .expect("right neighbor");
        store.replace_exact_pair_with(
            left,
            right,
            FreeSpan {
                start_slot: COALESCE_START,
                len: COALESCE_LEN * 2,
            },
        );
        black_box(store.len());
    })
}

fn bench_current_mixed_alloc_release(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_current_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        for _ in 0..MIXED_OPS {
            let span = store
                .take_best_fit(black_box(MIN_LEN))
                .expect("eligible span");
            store.release(span);
            black_box(span.start_slot);
        }
    })
}

fn bench_dual_index_take_best_fit(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let mut store = populate_dual_index_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let span = store
            .take_best_fit_whole(black_box(MIN_LEN))
            .expect("take_best_fit_whole")
            .expect("eligible span");
        store.release(span).expect("restore span");
        black_box(span.start_slot);
    })
}

fn bench_dual_index_release_coalescing(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let mut store = populate_dual_index_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        store
            .release_span(COALESCE_START + COALESCE_LEN, COALESCE_LEN)
            .expect("release coalescing");
        black_box(store.len());
    })
}

fn bench_dual_index_mixed_alloc_release(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let mut store = populate_dual_index_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        for _ in 0..MIXED_OPS {
            let span = store
                .take_best_fit(black_box(MIN_LEN))
                .expect("take_best_fit")
                .expect("eligible span");
            store.release(span).expect("restore span");
            black_box(span.start_slot);
        }
    })
}

fn bench_binned_take_best_fit_whole(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_binned_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let span = store
            .take_best_fit_whole(black_box(MIN_LEN))
            .expect("take_best_fit_whole")
            .expect("eligible span");
        store.release(span).expect("restore span");
        black_box(span.start_slot);
    })
}

fn bench_binned_take_best_fit_split(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_binned_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let span = store
            .take_best_fit(black_box(MIN_LEN))
            .expect("take_best_fit")
            .expect("eligible span");
        store.release(span).expect("restore span");
        black_box(span.start_slot);
    })
}

fn bench_binned_neighbor_lookup(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_binned_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let left = store
            .free_span_ending_at(black_box(COALESCE_START + COALESCE_LEN))
            .expect("left neighbor");
        let right = store
            .free_span_starting_at(black_box(COALESCE_START + COALESCE_LEN * 2))
            .expect("right neighbor");
        black_box(left.start_slot ^ right.start_slot);
    })
}

fn bench_binned_release_coalescing(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_binned_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        store
            .release_span(COALESCE_START + COALESCE_LEN, COALESCE_LEN)
            .expect("release coalescing");
        black_box(store.len());
    })
}

fn bench_binned_replace_exact_pair(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_binned_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let left = store
            .free_span_ending_at(black_box(COALESCE_START + COALESCE_LEN))
            .expect("left neighbor");
        let right = store
            .free_span_starting_at(black_box(COALESCE_START + COALESCE_LEN * 2))
            .expect("right neighbor");
        store
            .replace_exact_pair_with(
                left,
                right,
                FreeSpan {
                    start_slot: COALESCE_START,
                    len: COALESCE_LEN * 2,
                },
            )
            .expect("replace_exact_pair_with");
        black_box(store.len());
    })
}

fn bench_binned_mixed_alloc_release(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_binned_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        for _ in 0..MIXED_OPS {
            let span = store
                .take_best_fit(black_box(MIN_LEN))
                .expect("take_best_fit")
                .expect("eligible span");
            store
                .restore_allocated_prefix(span)
                .expect("restore allocated prefix");
            black_box(span.start_slot);
        }
    })
}

fn bench_binned_btree_neighbor_lookup(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_binned_btree_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let left = store
            .free_span_ending_at(black_box(COALESCE_START + COALESCE_LEN))
            .expect("left neighbor");
        let right = store
            .free_span_starting_at(black_box(COALESCE_START + COALESCE_LEN * 2))
            .expect("right neighbor");
        black_box(left.start_slot ^ right.start_slot);
    })
}

fn bench_binned_btree_release_coalescing(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_binned_btree_store_for_coalesce(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        store
            .release_span(COALESCE_START + COALESCE_LEN, COALESCE_LEN)
            .expect("release coalescing");
        black_box(store.len());
    })
}

fn bench_binned_btree_mixed_alloc_release(n: u64, scope: &'static str) -> canbench_rs::BenchResult {
    let store = populate_binned_btree_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        for _ in 0..MIXED_OPS {
            let span = store
                .take_best_fit(black_box(MIN_LEN))
                .expect("take_best_fit")
                .expect("eligible span");
            store
                .restore_allocated_prefix(span)
                .expect("restore allocated prefix");
            black_box(span.start_slot);
        }
    })
}

#[bench(raw)]
fn bench_lara_free_span_array_take_best_fit_256() -> canbench_rs::BenchResult {
    bench_array_take_best_fit(256, "lara_free_span_array_take_best_fit")
}

#[bench(raw)]
fn bench_lara_free_span_array_take_best_fit_1024() -> canbench_rs::BenchResult {
    bench_array_take_best_fit(1024, "lara_free_span_array_take_best_fit")
}

#[bench(raw)]
fn bench_lara_free_span_array_take_best_fit_4096() -> canbench_rs::BenchResult {
    bench_array_take_best_fit(4096, "lara_free_span_array_take_best_fit")
}

#[bench(raw)]
fn bench_lara_free_span_btree_take_best_fit_256() -> canbench_rs::BenchResult {
    bench_btree_take_best_fit(256, "lara_free_span_btree_take_best_fit")
}

#[bench(raw)]
fn bench_lara_free_span_btree_take_best_fit_1024() -> canbench_rs::BenchResult {
    bench_btree_take_best_fit(1024, "lara_free_span_btree_take_best_fit")
}

#[bench(raw)]
fn bench_lara_free_span_btree_take_best_fit_4096() -> canbench_rs::BenchResult {
    bench_btree_take_best_fit(4096, "lara_free_span_btree_take_best_fit")
}

#[bench(raw)]
fn bench_lara_free_span_array_release_coalescing_256() -> canbench_rs::BenchResult {
    bench_array_release_coalescing(256, "lara_free_span_array_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_array_release_coalescing_1024() -> canbench_rs::BenchResult {
    bench_array_release_coalescing(1024, "lara_free_span_array_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_array_release_coalescing_4096() -> canbench_rs::BenchResult {
    bench_array_release_coalescing(4096, "lara_free_span_array_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_store_take_best_fit_whole_256() -> canbench_rs::BenchResult {
    bench_current_take_best_fit_whole(256, "lara_free_span_store_take_best_fit_whole")
}

#[bench(raw)]
fn bench_lara_free_span_store_take_best_fit_whole_1024() -> canbench_rs::BenchResult {
    bench_current_take_best_fit_whole(1024, "lara_free_span_store_take_best_fit_whole")
}

#[bench(raw)]
fn bench_lara_free_span_store_take_best_fit_whole_4096() -> canbench_rs::BenchResult {
    bench_current_take_best_fit_whole(4096, "lara_free_span_store_take_best_fit_whole")
}

#[bench(raw)]
fn bench_lara_free_span_store_take_best_fit_split_256() -> canbench_rs::BenchResult {
    bench_current_take_best_fit_split(256, "lara_free_span_store_take_best_fit_split")
}

#[bench(raw)]
fn bench_lara_free_span_store_take_best_fit_split_1024() -> canbench_rs::BenchResult {
    bench_current_take_best_fit_split(1024, "lara_free_span_store_take_best_fit_split")
}

#[bench(raw)]
fn bench_lara_free_span_store_take_best_fit_split_4096() -> canbench_rs::BenchResult {
    bench_current_take_best_fit_split(4096, "lara_free_span_store_take_best_fit_split")
}

#[bench(raw)]
fn bench_lara_free_span_store_neighbor_lookup_256() -> canbench_rs::BenchResult {
    bench_current_neighbor_lookup(256, "lara_free_span_store_neighbor_lookup")
}

#[bench(raw)]
fn bench_lara_free_span_store_neighbor_lookup_1024() -> canbench_rs::BenchResult {
    bench_current_neighbor_lookup(1024, "lara_free_span_store_neighbor_lookup")
}

#[bench(raw)]
fn bench_lara_free_span_store_neighbor_lookup_4096() -> canbench_rs::BenchResult {
    bench_current_neighbor_lookup(4096, "lara_free_span_store_neighbor_lookup")
}

#[bench(raw)]
fn bench_lara_free_span_store_release_coalescing_256() -> canbench_rs::BenchResult {
    bench_current_release_coalescing(256, "lara_free_span_store_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_store_release_coalescing_1024() -> canbench_rs::BenchResult {
    bench_current_release_coalescing(1024, "lara_free_span_store_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_store_release_coalescing_4096() -> canbench_rs::BenchResult {
    bench_current_release_coalescing(4096, "lara_free_span_store_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_store_replace_exact_pair_256() -> canbench_rs::BenchResult {
    bench_current_replace_exact_pair(256, "lara_free_span_store_replace_exact_pair")
}

#[bench(raw)]
fn bench_lara_free_span_store_replace_exact_pair_1024() -> canbench_rs::BenchResult {
    bench_current_replace_exact_pair(1024, "lara_free_span_store_replace_exact_pair")
}

#[bench(raw)]
fn bench_lara_free_span_store_replace_exact_pair_4096() -> canbench_rs::BenchResult {
    bench_current_replace_exact_pair(4096, "lara_free_span_store_replace_exact_pair")
}

#[bench(raw)]
fn bench_lara_free_span_store_mixed_alloc_release_256() -> canbench_rs::BenchResult {
    bench_current_mixed_alloc_release(256, "lara_free_span_store_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_store_mixed_alloc_release_1024() -> canbench_rs::BenchResult {
    bench_current_mixed_alloc_release(1024, "lara_free_span_store_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_store_mixed_alloc_release_4096() -> canbench_rs::BenchResult {
    bench_current_mixed_alloc_release(4096, "lara_free_span_store_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_dual_index_take_best_fit_256() -> canbench_rs::BenchResult {
    bench_dual_index_take_best_fit(256, "lara_free_span_dual_index_take_best_fit")
}

#[bench(raw)]
fn bench_lara_free_span_dual_index_take_best_fit_1024() -> canbench_rs::BenchResult {
    bench_dual_index_take_best_fit(1024, "lara_free_span_dual_index_take_best_fit")
}

#[bench(raw)]
fn bench_lara_free_span_dual_index_take_best_fit_4096() -> canbench_rs::BenchResult {
    bench_dual_index_take_best_fit(4096, "lara_free_span_dual_index_take_best_fit")
}

#[bench(raw)]
fn bench_lara_free_span_dual_index_release_coalescing_256() -> canbench_rs::BenchResult {
    bench_dual_index_release_coalescing(256, "lara_free_span_dual_index_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_dual_index_release_coalescing_1024() -> canbench_rs::BenchResult {
    bench_dual_index_release_coalescing(1024, "lara_free_span_dual_index_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_dual_index_release_coalescing_4096() -> canbench_rs::BenchResult {
    bench_dual_index_release_coalescing(4096, "lara_free_span_dual_index_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_dual_index_mixed_alloc_release_256() -> canbench_rs::BenchResult {
    bench_dual_index_mixed_alloc_release(256, "lara_free_span_dual_index_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_dual_index_mixed_alloc_release_1024() -> canbench_rs::BenchResult {
    bench_dual_index_mixed_alloc_release(1024, "lara_free_span_dual_index_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_dual_index_mixed_alloc_release_4096() -> canbench_rs::BenchResult {
    bench_dual_index_mixed_alloc_release(4096, "lara_free_span_dual_index_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_binned_btree_neighbor_lookup_256() -> canbench_rs::BenchResult {
    bench_binned_btree_neighbor_lookup(256, "lara_free_span_binned_btree_neighbor_lookup")
}

#[bench(raw)]
fn bench_lara_free_span_binned_btree_neighbor_lookup_1024() -> canbench_rs::BenchResult {
    bench_binned_btree_neighbor_lookup(1024, "lara_free_span_binned_btree_neighbor_lookup")
}

#[bench(raw)]
fn bench_lara_free_span_binned_btree_neighbor_lookup_4096() -> canbench_rs::BenchResult {
    bench_binned_btree_neighbor_lookup(4096, "lara_free_span_binned_btree_neighbor_lookup")
}

#[bench(raw)]
fn bench_lara_free_span_binned_btree_release_coalescing_256() -> canbench_rs::BenchResult {
    bench_binned_btree_release_coalescing(256, "lara_free_span_binned_btree_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_binned_btree_release_coalescing_1024() -> canbench_rs::BenchResult {
    bench_binned_btree_release_coalescing(1024, "lara_free_span_binned_btree_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_binned_btree_release_coalescing_4096() -> canbench_rs::BenchResult {
    bench_binned_btree_release_coalescing(4096, "lara_free_span_binned_btree_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_binned_btree_mixed_alloc_release_256() -> canbench_rs::BenchResult {
    bench_binned_btree_mixed_alloc_release(256, "lara_free_span_binned_btree_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_binned_btree_mixed_alloc_release_1024() -> canbench_rs::BenchResult {
    bench_binned_btree_mixed_alloc_release(1024, "lara_free_span_binned_btree_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_binned_btree_mixed_alloc_release_4096() -> canbench_rs::BenchResult {
    bench_binned_btree_mixed_alloc_release(4096, "lara_free_span_binned_btree_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_binned_take_best_fit_whole_256() -> canbench_rs::BenchResult {
    bench_binned_take_best_fit_whole(256, "lara_free_span_binned_take_best_fit_whole")
}

#[bench(raw)]
fn bench_lara_free_span_binned_take_best_fit_whole_1024() -> canbench_rs::BenchResult {
    bench_binned_take_best_fit_whole(1024, "lara_free_span_binned_take_best_fit_whole")
}

#[bench(raw)]
fn bench_lara_free_span_binned_take_best_fit_whole_4096() -> canbench_rs::BenchResult {
    bench_binned_take_best_fit_whole(4096, "lara_free_span_binned_take_best_fit_whole")
}

#[bench(raw)]
fn bench_lara_free_span_binned_take_best_fit_split_256() -> canbench_rs::BenchResult {
    bench_binned_take_best_fit_split(256, "lara_free_span_binned_take_best_fit_split")
}

#[bench(raw)]
fn bench_lara_free_span_binned_take_best_fit_split_1024() -> canbench_rs::BenchResult {
    bench_binned_take_best_fit_split(1024, "lara_free_span_binned_take_best_fit_split")
}

#[bench(raw)]
fn bench_lara_free_span_binned_take_best_fit_split_4096() -> canbench_rs::BenchResult {
    bench_binned_take_best_fit_split(4096, "lara_free_span_binned_take_best_fit_split")
}

#[bench(raw)]
fn bench_lara_free_span_binned_neighbor_lookup_256() -> canbench_rs::BenchResult {
    bench_binned_neighbor_lookup(256, "lara_free_span_binned_neighbor_lookup")
}

#[bench(raw)]
fn bench_lara_free_span_binned_neighbor_lookup_1024() -> canbench_rs::BenchResult {
    bench_binned_neighbor_lookup(1024, "lara_free_span_binned_neighbor_lookup")
}

#[bench(raw)]
fn bench_lara_free_span_binned_neighbor_lookup_4096() -> canbench_rs::BenchResult {
    bench_binned_neighbor_lookup(4096, "lara_free_span_binned_neighbor_lookup")
}

#[bench(raw)]
fn bench_lara_free_span_binned_release_coalescing_256() -> canbench_rs::BenchResult {
    bench_binned_release_coalescing(256, "lara_free_span_binned_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_binned_release_coalescing_1024() -> canbench_rs::BenchResult {
    bench_binned_release_coalescing(1024, "lara_free_span_binned_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_binned_release_coalescing_4096() -> canbench_rs::BenchResult {
    bench_binned_release_coalescing(4096, "lara_free_span_binned_release_coalescing")
}

#[bench(raw)]
fn bench_lara_free_span_binned_replace_exact_pair_256() -> canbench_rs::BenchResult {
    bench_binned_replace_exact_pair(256, "lara_free_span_binned_replace_exact_pair")
}

#[bench(raw)]
fn bench_lara_free_span_binned_replace_exact_pair_1024() -> canbench_rs::BenchResult {
    bench_binned_replace_exact_pair(1024, "lara_free_span_binned_replace_exact_pair")
}

#[bench(raw)]
fn bench_lara_free_span_binned_replace_exact_pair_4096() -> canbench_rs::BenchResult {
    bench_binned_replace_exact_pair(4096, "lara_free_span_binned_replace_exact_pair")
}

#[bench(raw)]
fn bench_lara_free_span_binned_mixed_alloc_release_256() -> canbench_rs::BenchResult {
    bench_binned_mixed_alloc_release(256, "lara_free_span_binned_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_binned_mixed_alloc_release_1024() -> canbench_rs::BenchResult {
    bench_binned_mixed_alloc_release(1024, "lara_free_span_binned_mixed_alloc_release")
}

#[bench(raw)]
fn bench_lara_free_span_binned_mixed_alloc_release_4096() -> canbench_rs::BenchResult {
    bench_binned_mixed_alloc_release(4096, "lara_free_span_binned_mixed_alloc_release")
}
