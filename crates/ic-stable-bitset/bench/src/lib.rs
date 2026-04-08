//! PocketIC / `canbench` harness for [`ic_stable_bitset::BitSet`].
//!
//! See the local `README.md` for build and run commands.

#![cfg_attr(target_arch = "wasm32", no_main)]

use std::hint::black_box;

use canbench_rs::bench;
use ic_cdk::export_candid;
use ic_cdk_macros::{init, post_upgrade, pre_upgrade};
use ic_stable_bitset::BitSet;
use ic_stable_structures::DefaultMemoryImpl;

mod wipe;

const INSERT_COUNT: u64 = 1_024;
const TRUNCATE_FROM: u64 = 2_048;
const TRUNCATE_TO: u64 = 1_024;
const REOPEN_COUNT: u64 = 4_096;
const LARGE_SNAPSHOT_BITS: u64 = 65_536;
const LARGE_TRUNCATE_FROM: u64 = 65_536;
const LARGE_TRUNCATE_TO: u64 = 32_768;
const JOURNAL_CAP_FILL: u64 = 4_096;

fn make_bitset() -> BitSet<DefaultMemoryImpl> {
    BitSet::new(DefaultMemoryImpl::default()).expect("bitset init")
}

fn populate(bitset: &BitSet<DefaultMemoryImpl>, count: u64) {
    for index in 0..count {
        bitset.insert(index).expect("insert");
    }
}

#[bench(raw)]
fn bench_bitset_insert_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    canbench_rs::bench_fn(|| {
        let bitset = make_bitset();
        let _p = canbench_rs::bench_scope("bitset_insert");
        populate(&bitset, INSERT_COUNT);
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
        black_box(bitset.contains(INSERT_COUNT - 1));
    })
}

#[bench(raw)]
fn bench_bitset_truncate_2048_to_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, TRUNCATE_FROM);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_truncate");
        bitset.truncate(TRUNCATE_TO).expect("truncate");
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
        black_box(reopened.contains(REOPEN_COUNT));
    })
}

#[bench(raw)]
fn bench_bitset_truncate_large_suffix_65536_to_32768() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, LARGE_TRUNCATE_FROM);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_truncate_large");
        bitset.truncate(LARGE_TRUNCATE_TO).expect("truncate");
        black_box(bitset.len());
    })
}

#[bench(raw)]
fn bench_bitset_checkpoint_after_full_journal_4096() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let bitset = make_bitset();
    populate(&bitset, JOURNAL_CAP_FILL);
    canbench_rs::bench_fn(|| {
        let _p = canbench_rs::bench_scope("bitset_checkpoint");
        bitset
            .insert(JOURNAL_CAP_FILL)
            .expect("insert triggering checkpoint");
        black_box(bitset.contains(JOURNAL_CAP_FILL));
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
        black_box(reopened.contains(LARGE_SNAPSHOT_BITS));
    })
}

#[init]
fn init() {}

#[pre_upgrade]
fn pre_upgrade() {}

#[post_upgrade]
fn post_upgrade() {}

export_candid!();
