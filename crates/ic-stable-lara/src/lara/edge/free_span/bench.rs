use std::hint::black_box;

use canbench_rs::bench;

use super::{FreeSpan, FreeSpanStore};
use crate::bench as helper;

const MIN_LEN: u64 = 96;

/// Size-class bin `[65, 128]` parameters for the bounded-scan fallback benchmark.
/// All three lengths share one size class, so the search stays in a single bin.
const FALLBACK_FIT_LEN: u64 = 120;
const FALLBACK_MISS_LEN: u64 = 99;
const FALLBACK_MIN_LEN: u64 = 100;

fn random_span(i: u64) -> FreeSpan {
    let seed = helper::splitmix64(i ^ 0x243F_6A88_85A3_08D3);
    FreeSpan {
        start_slot: i.saturating_mul(1_000_000).saturating_add(seed % 997),
        len: MIN_LEN + (seed % 4096),
    }
}

fn populate_store(n: u64) -> FreeSpanStore<helper::BenchMemory> {
    let mut memories = helper::BenchMemoryFactory::new();
    let store = FreeSpanStore::new(memories.memory(), memories.memory()).expect("free span store");
    for i in 0..n {
        store.release(random_span(i)).expect("release span");
    }
    store
}

fn bench_take_best_fit_split(n: u64) -> canbench_rs::BenchResult {
    let store = populate_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_free_span_store_take_best_fit_split");
        for i in 0..16 {
            let i = black_box(i);
            let min_len = black_box(MIN_LEN + i);
            let span = store
                .take_best_fit(min_len)
                .expect("take best fit")
                .expect("span");
            store
                .restore_allocated_prefix(span)
                .expect("restore allocated prefix");
        }
    })
}

/// Measures best-fit allocation with split and immediate prefix restoration in
/// a 256-span free-list population. This preserves the historical baseline name
/// for tracking the small-population allocator path.
#[bench(raw)]
fn bench_lara_free_span_store_take_best_fit_split_256() -> canbench_rs::BenchResult {
    bench_take_best_fit_split(helper::SMALL_N)
}

/// Measures best-fit split/restore with a medium population. This is the main
/// regression signal for bin scanning, by-start updates, and active-record
/// relinking under typical allocator pressure.
#[bench(raw)]
fn bench_lara_free_span_store_take_best_fit_split_1024() -> canbench_rs::BenchResult {
    bench_take_best_fit_split(helper::MEDIUM_N)
}

/// Measures best-fit split/restore with a large population. The target is to
/// keep allocation cost mostly tied to bin locality rather than total free-span
/// count.
#[bench(raw)]
fn bench_lara_free_span_store_take_best_fit_split_4096() -> canbench_rs::BenchResult {
    bench_take_best_fit_split(helper::LARGE_N)
}

/// Builds a store whose start size-class bin holds one fitting span at the
/// linked-list tail, buried behind `n - 1` shorter, non-fitting spans in the
/// same bin. This is the layout that defeats the bounded best-fit scan and
/// forces the first-fit fallback to walk the whole bin.
fn populate_bin_fallback_store(n: u64) -> FreeSpanStore<helper::BenchMemory> {
    let mut memories = helper::BenchMemoryFactory::new();
    let store = FreeSpanStore::new(memories.memory(), memories.memory()).expect("free span store");
    // Released first, so it ends up at the bin-list tail (releases prepend).
    store
        .release_span(0, FALLBACK_FIT_LEN)
        .expect("release fitting span");
    for i in 1..n {
        store
            .release_span(i.saturating_mul(1_000_000), FALLBACK_MISS_LEN)
            .expect("release non-fitting span");
    }
    store
}

/// Measures the bounded-scan first-fit fallback: every probe scans past the
/// non-fitting head window and walks to the tail of the start bin. The stable
/// read count is expected to grow with bin occupancy, so the 256 and 4096
/// variants together track that linear cost (the prior best-fit benches never
/// exercise this path because all their spans fit within the head window).
fn bench_best_fit_fallback_scan(n: u64) -> canbench_rs::BenchResult {
    let store = populate_bin_fallback_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_free_span_store_best_fit_fallback_scan");
        for _ in 0..16 {
            let span = store.peek_best_fit(black_box(FALLBACK_MIN_LEN));
            black_box(span);
        }
    })
}

#[bench(raw)]
fn bench_lara_free_span_store_best_fit_fallback_scan_256() -> canbench_rs::BenchResult {
    bench_best_fit_fallback_scan(helper::SMALL_N)
}

#[bench(raw)]
fn bench_lara_free_span_store_best_fit_fallback_scan_4096() -> canbench_rs::BenchResult {
    bench_best_fit_fallback_scan(helper::LARGE_N)
}

/// Builds one size-class bin containing one largest span and many smaller
/// spans. Removing the largest span forces the allocator to rebuild its cached
/// maximum from that bin; the measurement therefore tracks the remaining
/// largest-bin scan rather than the old all-spans by-start scan.
fn populate_largest_bin_store(n: u64) -> FreeSpanStore<helper::BenchMemory> {
    let mut memories = helper::BenchMemoryFactory::new();
    let store = FreeSpanStore::new(memories.memory(), memories.memory()).expect("free span store");
    store.release_span(0, 128).expect("release largest span");
    for i in 1..n {
        store
            .release_span(i.saturating_mul(1_000_000), 96)
            .expect("release same-bin span");
    }
    store
}

fn bench_largest_bin_recovery(n: u64) -> canbench_rs::BenchResult {
    let store = populate_largest_bin_store(n);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_free_span_largest_bin_recovery");
        for _ in 0..16 {
            let largest = store
                .take_best_fit_whole(black_box(128))
                .expect("take largest span")
                .expect("largest span");
            store.release(largest).expect("restore largest span");
        }
    })
}

#[bench(raw)]
fn bench_lara_free_span_store_largest_bin_recovery_256() -> canbench_rs::BenchResult {
    bench_largest_bin_recovery(helper::SMALL_N)
}

#[bench(raw)]
fn bench_lara_free_span_store_largest_bin_recovery_4096() -> canbench_rs::BenchResult {
    bench_largest_bin_recovery(helper::LARGE_N)
}

/// Measures the cost of reopening a populated free-span store. `init` runs the
/// full reopen integrity sequence: `by_start.validate`, the
/// `by_start.len == active_count` cross-check, and `FreeSpanStore::validate`.
/// The population uses well-separated, non-coalescing spans so `active_count`
/// equals `n`, which makes this the primary signal for reopen-path validation
/// cost as the live free-span set grows.
fn bench_reopen(n: u64) -> canbench_rs::BenchResult {
    let (store_mem, by_start_mem) = populate_store(n).into_memories();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_free_span_store_reopen");
        let reopened = FreeSpanStore::init(store_mem.clone(), by_start_mem.clone())
            .expect("reopen free span store");
        black_box(reopened.len());
    })
}

#[bench(raw)]
fn bench_lara_free_span_store_reopen_256() -> canbench_rs::BenchResult {
    bench_reopen(helper::SMALL_N)
}

#[bench(raw)]
fn bench_lara_free_span_store_reopen_1024() -> canbench_rs::BenchResult {
    bench_reopen(helper::MEDIUM_N)
}

#[bench(raw)]
fn bench_lara_free_span_store_reopen_4096() -> canbench_rs::BenchResult {
    bench_reopen(helper::LARGE_N)
}

/// Measures release-time coalescing of adjacent free spans in the current
/// store. This protects predecessor/successor lookup plus replacement of
/// neighboring records with a merged reusable range.
#[bench(raw)]
fn bench_lara_free_span_release_coalesce_1024() -> canbench_rs::BenchResult {
    let store = populate_store(helper::MEDIUM_N);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_free_span_release_coalesce");
        let base = black_box(9_000_000_000);
        store.release_span(base, 64).expect("left span");
        store.release_span(base + 128, 64).expect("right span");
        let taken = store
            .take_best_fit(black_box(192))
            .expect("take merged")
            .expect("merged span");
        store
            .restore_allocated_prefix(black_box(taken))
            .expect("restore merged");
    })
}
