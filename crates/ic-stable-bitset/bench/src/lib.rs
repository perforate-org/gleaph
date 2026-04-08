//! PocketIC / `canbench` harness for [`ic_stable_bitset::BitSet`].
//!
//! See the local `README.md` for build and run commands.

#![cfg_attr(target_arch = "wasm32", no_main)]

use std::hint::black_box;

use canbench_rs::bench;
use ic_stable_bitset::BitSet;
use ic_stable_structures::DefaultMemoryImpl;

mod wipe;

const INSERT_COUNT: u64 = 1_024;
const TRUNCATE_FROM: u64 = 2_048;
const TRUNCATE_TO: u64 = 1_024;
const REOPEN_COUNT: u64 = 4_096;
const LARGE_SNAPSHOT_BITS: u64 = 65_536;
const CONTAINS_BITMAP_BITS: u64 = 65_536;
const CONTAINS_QUERY_COUNT: u64 = 4_096;
const CONTAINS_SPREAD_MULTIPLIER: u64 = 0x9E37;
const CONTAINS_SPREAD_INCREMENT: u64 = 0xB529;
const LARGE_TRUNCATE_FROM: u64 = 65_536;
const LARGE_TRUNCATE_TO: u64 = 32_768;
const JOURNAL_CAP_FILL: u64 = 4_096;
const REPLAY_BLOCK: u64 = JOURNAL_CAP_FILL / 4;
const REPLAY_HALF: u64 = JOURNAL_CAP_FILL / 2;
const REMOVE_SMALL_BITS: u64 = 1_024;
const REMOVE_SMALL_HEAD: u64 = 0;
const REMOVE_SMALL_MID: u64 = 512;
const REMOVE_SMALL_TAIL: u64 = REMOVE_SMALL_BITS - 1;
const REMOVE_LARGE_BITS: u64 = 65_536;
const REMOVE_LARGE_HEAD: u64 = 0;
const REMOVE_LARGE_MID: u64 = 32_768;
const REMOVE_LARGE_TAIL: u64 = REMOVE_LARGE_BITS - 1;

fn make_bitset() -> BitSet<DefaultMemoryImpl> {
    BitSet::new(DefaultMemoryImpl::default()).expect("bitset init")
}

fn populate(bitset: &BitSet<DefaultMemoryImpl>, count: u64) {
    for index in 0..count {
        bitset.insert(index).expect("insert");
    }
}

fn bench_remove_case(
    scope_name: &'static str,
    count: u64,
    remove_index: u64,
) -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, count);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope(scope_name);
        bitset.remove(black_box(remove_index)).expect("remove");
        black_box(bitset.len());
    })
}

fn bench_reopen_case(
    scope_name: &'static str,
    setup: impl FnOnce(&BitSet<DefaultMemoryImpl>),
) -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    setup(&bitset);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope(scope_name);
        let reopened = BitSet::init(bitset.into_memory()).expect("reopen");
        black_box((reopened.len(), reopened.contains(black_box(0))));
    })
}

fn setup_pure_replay_journal(bitset: &BitSet<DefaultMemoryImpl>) {
    for index in 0..REPLAY_HALF {
        bitset.insert(index).expect("insert");
    }
    for index in 0..REPLAY_HALF {
        bitset.clear(index).expect("clear");
    }
}

fn setup_segmented_replay_journal(bitset: &BitSet<DefaultMemoryImpl>) {
    for index in 0..REPLAY_BLOCK {
        bitset.insert(index).expect("insert");
    }
    for index in 0..REPLAY_BLOCK {
        bitset.clear(index).expect("clear");
    }
    for index in 0..REPLAY_BLOCK {
        bitset.insert(index).expect("insert");
    }
    for _ in 0..REPLAY_BLOCK {
        bitset.remove(0).expect("remove");
    }
}

fn setup_remove_heavy_replay_journal(bitset: &BitSet<DefaultMemoryImpl>) {
    for index in 0..REPLAY_HALF {
        bitset.insert(index).expect("insert");
    }
    for _ in 0..REPLAY_HALF {
        bitset.remove(0).expect("remove");
    }
    for index in 0..REPLAY_HALF {
        bitset.insert(index).expect("insert");
    }
    for _ in 0..REPLAY_HALF {
        bitset.remove(0).expect("remove");
    }
}

fn make_spread_queries(count: u64, modulo: u64) -> Vec<u64> {
    assert!(
        modulo.is_power_of_two(),
        "bitmap size should be a power of two"
    );
    let mask = modulo - 1;
    let mut queries = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mixed = i
            .wrapping_mul(CONTAINS_SPREAD_MULTIPLIER)
            .wrapping_add(CONTAINS_SPREAD_INCREMENT);
        queries.push(mixed & mask);
    }
    queries
}

#[bench(raw)]
fn bench_bitset_insert_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    canbench_rs::bench_fn(|| {
        let bitset = make_bitset();
        let _p = canbench_rs::bench_scope("bitset_insert");
        populate(&bitset, black_box(INSERT_COUNT));
        black_box(bitset.len());
    })
}

#[bench(raw)]
fn bench_bitset_contains_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, INSERT_COUNT);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_contains");
        let index = black_box(INSERT_COUNT - 1);
        black_box(bitset.contains(index));
    })
}

#[bench(raw)]
fn bench_bitset_contains_65536_4096() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, CONTAINS_BITMAP_BITS);
    let queries = make_spread_queries(CONTAINS_QUERY_COUNT, CONTAINS_BITMAP_BITS);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_contains_large");
        let mut acc = false;
        for index in black_box(&queries) {
            acc ^= bitset.contains(*index);
        }
        black_box(acc);
    })
}

#[bench(raw)]
fn bench_bitset_truncate_2048_to_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, TRUNCATE_FROM);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_truncate");
        bitset.truncate(black_box(TRUNCATE_TO)).expect("truncate");
        black_box(bitset.len());
    })
}

#[bench(raw)]
fn bench_bitset_reopen_after_journal_4096() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, REOPEN_COUNT);
    bitset.insert(REOPEN_COUNT).expect("insert");
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_reopen");
        let reopened = BitSet::init(bitset.into_memory()).expect("reopen");
        black_box(reopened.contains(black_box(REOPEN_COUNT)));
    })
}

#[bench(raw)]
fn bench_bitset_reopen_after_pure_journal_4096() -> canbench_rs::BenchResult {
    bench_reopen_case("bitset_reopen_pure_journal", setup_pure_replay_journal)
}

#[bench(raw)]
fn bench_bitset_reopen_after_segmented_journal_4096() -> canbench_rs::BenchResult {
    bench_reopen_case(
        "bitset_reopen_segmented_journal",
        setup_segmented_replay_journal,
    )
}

#[bench(raw)]
fn bench_bitset_reopen_after_remove_heavy_journal_4096() -> canbench_rs::BenchResult {
    bench_reopen_case(
        "bitset_reopen_remove_heavy_journal",
        setup_remove_heavy_replay_journal,
    )
}

#[bench(raw)]
fn bench_bitset_truncate_large_suffix_65536_to_32768() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, LARGE_TRUNCATE_FROM);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_truncate_large");
        bitset
            .truncate(black_box(LARGE_TRUNCATE_TO))
            .expect("truncate");
        black_box(bitset.len());
    })
}

#[bench(raw)]
fn bench_bitset_remove_1024_head() -> canbench_rs::BenchResult {
    bench_remove_case(
        "bitset_remove_1024_head",
        REMOVE_SMALL_BITS,
        REMOVE_SMALL_HEAD,
    )
}

#[bench(raw)]
fn bench_bitset_remove_1024_mid() -> canbench_rs::BenchResult {
    bench_remove_case(
        "bitset_remove_1024_mid",
        REMOVE_SMALL_BITS,
        REMOVE_SMALL_MID,
    )
}

#[bench(raw)]
fn bench_bitset_remove_1024_tail() -> canbench_rs::BenchResult {
    bench_remove_case(
        "bitset_remove_1024_tail",
        REMOVE_SMALL_BITS,
        REMOVE_SMALL_TAIL,
    )
}

#[bench(raw)]
fn bench_bitset_remove_65536_head() -> canbench_rs::BenchResult {
    bench_remove_case(
        "bitset_remove_65536_head",
        REMOVE_LARGE_BITS,
        REMOVE_LARGE_HEAD,
    )
}

#[bench(raw)]
fn bench_bitset_remove_65536_mid() -> canbench_rs::BenchResult {
    bench_remove_case(
        "bitset_remove_65536_mid",
        REMOVE_LARGE_BITS,
        REMOVE_LARGE_MID,
    )
}

#[bench(raw)]
fn bench_bitset_remove_65536_tail() -> canbench_rs::BenchResult {
    bench_remove_case(
        "bitset_remove_65536_tail",
        REMOVE_LARGE_BITS,
        REMOVE_LARGE_TAIL,
    )
}

#[bench(raw)]
fn bench_bitset_checkpoint_after_full_journal_4096() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, JOURNAL_CAP_FILL);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_checkpoint");
        bitset
            .insert(black_box(JOURNAL_CAP_FILL))
            .expect("insert triggering checkpoint");
        black_box(bitset.contains(black_box(JOURNAL_CAP_FILL)));
    })
}

#[bench(raw)]
fn bench_bitset_reopen_after_large_snapshot_65536() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, LARGE_SNAPSHOT_BITS);
    bitset
        .insert(LARGE_SNAPSHOT_BITS)
        .expect("insert triggering checkpoint");
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_reopen_large");
        let reopened = BitSet::init(bitset.into_memory()).expect("reopen");
        black_box(reopened.contains(black_box(LARGE_SNAPSHOT_BITS)));
    })
}
