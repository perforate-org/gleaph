//! PocketIC / `canbench` harness for [`ic_stable_roaring::StableRoaringBitmap`].
//!
//! See the local `README.md` for build and run commands.

#![cfg_attr(target_arch = "wasm32", no_main)]

use std::hint::black_box;

use canbench_rs::bench;
use ic_stable_roaring::StableRoaringBitmap;
use ic_stable_structures::DefaultMemoryImpl;

mod wipe;

const INSERT_COUNT: u64 = 1_024;
const TRUNCATE_FROM: u64 = 2_048;
const TRUNCATE_TO: u64 = 1_024;
const REOPEN_COUNT: u64 = 4_096;
const LARGE_SNAPSHOT_BITS: u64 = 65_536;
const CONTAINS_BITMAP_BITS: u64 = 65_536;
const CONTAINS_QUERY_COUNT: u64 = 4_096;
const CONTAINS_QUERY_COUNT_LARGE: u64 = 32_768;
const CONTAINS_SPREAD_MULTIPLIER: u64 = 0x9E37;
const CONTAINS_SPREAD_INCREMENT: u64 = 0xB529;
const LARGE_TRUNCATE_FROM: u64 = 65_536;
const LARGE_TRUNCATE_TO: u64 = 32_768;
const JOURNAL_CAP_FILL: u64 = 4_096;
const REPLAY_BLOCK: u64 = JOURNAL_CAP_FILL / 4;
const REPLAY_HALF: u64 = JOURNAL_CAP_FILL / 2;

fn make_bitset() -> StableRoaringBitmap<DefaultMemoryImpl> {
    StableRoaringBitmap::new(DefaultMemoryImpl::default()).expect("bitmap init")
}

fn populate(bitset: &StableRoaringBitmap<DefaultMemoryImpl>, count: u64) {
    for index in 0..count {
        bitset.insert(index as u32).expect("insert");
    }
}

fn bench_reopen_case(
    scope_name: &'static str,
    setup: impl FnOnce(&StableRoaringBitmap<DefaultMemoryImpl>),
) -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    setup(&bitset);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope(scope_name);
        let reopened = StableRoaringBitmap::init(bitset.into_memory()).expect("reopen");
        black_box((reopened.len(), reopened.contains(black_box(0u32))));
    })
}

fn setup_pure_replay_journal(bitset: &StableRoaringBitmap<DefaultMemoryImpl>) {
    for index in 0..REPLAY_HALF {
        bitset.insert(index as u32).expect("insert");
    }
    for index in 0..REPLAY_HALF {
        bitset.clear(index as u32).expect("clear");
    }
}

fn setup_segmented_replay_journal(bitset: &StableRoaringBitmap<DefaultMemoryImpl>) {
    for index in 0..REPLAY_BLOCK {
        bitset.insert(index as u32).expect("insert");
    }
    for index in 0..REPLAY_BLOCK {
        bitset.clear(index as u32).expect("clear");
    }
    for index in 0..REPLAY_BLOCK {
        bitset.insert(index as u32).expect("insert");
    }
    bitset.ensure_len(REPLAY_BLOCK * 4).expect("ensure_len");
    bitset.truncate(REPLAY_BLOCK * 3).expect("truncate");
}

fn make_spread_queries(count: u64, modulo: u64) -> Vec<u32> {
    assert!(
        modulo.is_power_of_two(),
        "bitmap size should be a power of two"
    );
    assert!(modulo <= u32::MAX as u64 + 1);
    let mask = modulo - 1;
    let mut queries = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mixed = i
            .wrapping_mul(CONTAINS_SPREAD_MULTIPLIER)
            .wrapping_add(CONTAINS_SPREAD_INCREMENT);
        queries.push((mixed & mask) as u32);
    }
    queries
}

#[bench(raw)]
fn bench_roaring_insert_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    canbench_rs::bench_fn(|| {
        let bitset = make_bitset();
        let _p = canbench_rs::bench_scope("roaring_insert");
        populate(&bitset, black_box(INSERT_COUNT));
        black_box(bitset.len());
    })
}

#[bench(raw)]
fn bench_roaring_contains_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, INSERT_COUNT);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("roaring_contains");
        let index = black_box(INSERT_COUNT - 1);
        black_box(bitset.contains(index as u32));
    })
}

#[bench(raw)]
fn bench_roaring_contains_65536_4096() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, CONTAINS_BITMAP_BITS);
    let queries = make_spread_queries(CONTAINS_QUERY_COUNT, CONTAINS_BITMAP_BITS);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("roaring_contains_large");
        let mut acc = false;
        for index in black_box(&queries) {
            acc ^= bitset.contains(*index);
        }
        black_box(acc);
    })
}

#[bench(raw)]
fn bench_roaring_contains_65536_32768() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, CONTAINS_BITMAP_BITS);
    let queries = make_spread_queries(CONTAINS_QUERY_COUNT_LARGE, CONTAINS_BITMAP_BITS);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("roaring_contains_large_32768");
        let mut acc = false;
        for index in black_box(&queries) {
            acc ^= bitset.contains(*index);
        }
        black_box(acc);
    })
}

#[bench(raw)]
fn bench_roaring_truncate_2048_to_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, TRUNCATE_FROM);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("roaring_truncate");
        bitset.truncate(black_box(TRUNCATE_TO)).expect("truncate");
        black_box(bitset.len());
    })
}

#[bench(raw)]
fn bench_roaring_reopen_after_journal_4096() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, REOPEN_COUNT);
    bitset.insert(REOPEN_COUNT as u32).expect("insert");
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("roaring_reopen");
        let reopened = StableRoaringBitmap::init(bitset.into_memory()).expect("reopen");
        black_box(reopened.contains(black_box(REOPEN_COUNT as u32)));
    })
}

#[bench(raw)]
fn bench_roaring_reopen_after_pure_journal_4096() -> canbench_rs::BenchResult {
    bench_reopen_case("roaring_reopen_pure_journal", setup_pure_replay_journal)
}

#[bench(raw)]
fn bench_roaring_reopen_after_segmented_journal_4096() -> canbench_rs::BenchResult {
    bench_reopen_case(
        "roaring_reopen_segmented_journal",
        setup_segmented_replay_journal,
    )
}

#[bench(raw)]
fn bench_roaring_truncate_large_suffix_65536_to_32768() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, LARGE_TRUNCATE_FROM);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("roaring_truncate_large");
        bitset
            .truncate(black_box(LARGE_TRUNCATE_TO))
            .expect("truncate");
        black_box(bitset.len());
    })
}

#[bench(raw)]
fn bench_roaring_checkpoint_after_full_journal_4096() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, JOURNAL_CAP_FILL);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("roaring_checkpoint");
        bitset
            .insert(black_box(JOURNAL_CAP_FILL as u32))
            .expect("insert triggering checkpoint");
        black_box(bitset.contains(black_box(JOURNAL_CAP_FILL as u32)));
    })
}

#[bench(raw)]
fn bench_roaring_reopen_after_large_snapshot_65536() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, LARGE_SNAPSHOT_BITS);
    bitset
        .insert(LARGE_SNAPSHOT_BITS as u32)
        .expect("insert triggering checkpoint");
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("roaring_reopen_large");
        let reopened = StableRoaringBitmap::init(bitset.into_memory()).expect("reopen");
        black_box(reopened.contains(black_box(LARGE_SNAPSHOT_BITS as u32)));
    })
}
