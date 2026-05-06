use std::hint::black_box;

use canbench_rs::bench;

use super::{FreeSpan, FreeSpanStore};
use crate::{bench as helper, test_support::vector_memory};

const MIN_LEN: u64 = 96;

fn random_span(i: u64) -> FreeSpan {
    let seed = helper::splitmix64(i ^ 0x243F_6A88_85A3_08D3);
    FreeSpan {
        start_slot: i.saturating_mul(1_000_000).saturating_add(seed % 997),
        len: MIN_LEN + (seed % 4096),
    }
}

fn populate_store(n: u64) -> FreeSpanStore<crate::VectorMemory> {
    let store = FreeSpanStore::new(vector_memory(), vector_memory()).expect("free span store");
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
            .take_best_fit(192)
            .expect("take merged")
            .expect("merged span");
        store
            .restore_allocated_prefix(taken)
            .expect("restore merged");
    })
}
