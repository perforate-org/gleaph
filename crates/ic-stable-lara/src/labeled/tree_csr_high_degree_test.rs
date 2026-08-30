//! Plan 0313 / Step 3 amend: cargo-test-based high-degree sweep.
//!
//! **Why a cargo test instead of canbench:** the PocketIC wasm export name
//! budget (20,000 chars across all exported fn names) was saturated by the
//! pre-existing 141 canbench functions plus the new parity + Gate 3/4
//! benches. Adding `tcsr_1048576_<op>` arms to the canbench surface would
//! push the wasm above the budget and prevent the canister from installing.
//!
//! **What this test does:** measures the four headline parity rows
//! (`full_scan_descending`, `random_ordinal_access`, `insert_grow`,
//! `delete_half_by_slot_then_scan`) plus the three rows dropped from the
//! canbench surface (`prefix_scan_descending`, `counterpart_resolve`,
//! `compaction`) at the **1M** degree. Wall-clock per-edge cost on the host
//! (not PocketIC instruction counts) so the test compiles against the
//! regular `cargo test` target.
//!
//! **Output:** the test prints `ins_per_edge = ns / edge_count` for each
//! row. The numbers are wall-clock proxy values, not PocketIC instructions,
//! and are recorded in `design/implementation-gaps.md` as the high-degree
//! anchor that the canbench 4K / 65K arms cannot reach under the wasm
//! budget. They are advisory and do not replace PocketIC instruction
//! numbers from canbench.
//!
//! **Plan 0315 amend (2026-08-30):** every `high_degree_*` test is
//! `#[ignore]`d. The raw-block `LtbRawBlockStore::mint` grows by
//! [`BLOCK_STRIDE`] = 4112 bytes per block; 1,048,576 mints exhaust the
//! `VectorMemory` test backend's process heap before the bench can finish.
//! On real ICP stable memory the same growth pattern is supported (the
//! canister's 32 GiB stable page budget fits > 1M blocks). The benches
//! must move to a PocketIC-backed target — that is a later slice. Plan
//! 0315's verdict relies on the 4K / 65K canbench arms, which do not hit
//! the same limit.

use crate::labeled::tree_csr_prototype::TreeCsrBucket;
use crate::test_support::vector_memory;
use std::hint::black_box;
use std::time::Instant;

const HIGH_DEG: u32 = 1_048_576;
const REPEAT: u32 = 1; // 1M-edge seeding dominates wall-clock; single pass.

fn seed_high_degree_hub() -> TreeCsrBucket<crate::VectorMemory> {
    let mut bucket = TreeCsrBucket::new(vector_memory());
    for i in 0..HIGH_DEG {
        bucket.insert(i);
    }
    bucket
}

fn ns_per_edge(label: &str, edges: u32, elapsed_ns: u128) {
    let per_edge = elapsed_ns / edges as u128;
    eprintln!(
        "[tree_csr_high_degree] {label}: {elapsed_ns} ns total / {edges} edges \
         = {per_edge} ns/edge (host wall-clock proxy)"
    );
}

#[test]
#[ignore = "Plan 0315: raw-block mint grow hits VectorMemory limit at 1M; deferred per implementation note"]
fn high_degree_full_scan_descending() {
    let bucket = seed_high_degree_hub();
    let started = Instant::now();
    let mut count: u64 = 0;
    for _ in 0..REPEAT {
        bucket.for_each_descending(|_slot, target| {
            count = count.wrapping_add(target as u64);
        });
    }
    black_box(count);
    let elapsed = started.elapsed().as_nanos();
    ns_per_edge("full_scan_descending", HIGH_DEG, elapsed);
}

#[test]
#[ignore = "Plan 0315: raw-block mint grow hits VectorMemory limit at 1M; deferred per implementation note"]
fn high_degree_random_ordinal_access() {
    let bucket = seed_high_degree_hub();
    let started = Instant::now();
    let mut acc: u64 = 0;
    for _ in 0..REPEAT {
        bucket.random_ordinal_access(64, |_slot, target| {
            acc = acc.wrapping_add(target as u64);
        });
    }
    black_box(acc);
    let elapsed = started.elapsed().as_nanos();
    ns_per_edge("random_ordinal_access", HIGH_DEG, elapsed);
}

#[test]
#[ignore = "Plan 0315: raw-block mint grow hits VectorMemory limit at 1M; deferred per implementation note"]
fn high_degree_insert_grow() {
    let started = Instant::now();
    let mut bucket = TreeCsrBucket::new(vector_memory());
    for target in 0..HIGH_DEG {
        bucket.insert(black_box(target));
    }
    black_box(bucket.stored_slots());
    let elapsed = started.elapsed().as_nanos();
    ns_per_edge("insert_grow", HIGH_DEG, elapsed);
}

#[test]
#[ignore = "Plan 0315: raw-block mint grow hits VectorMemory limit at 1M; deferred per implementation note"]
fn high_degree_delete_half_by_slot_then_scan() {
    let mut bucket = seed_high_degree_hub();
    let started = Instant::now();
    for slot in (0..HIGH_DEG).step_by(2) {
        bucket.remove_at(slot);
    }
    let mut count: u64 = 0;
    bucket.for_each_descending(|_slot, target| {
        count = count.wrapping_add(target as u64);
    });
    black_box(count);
    let elapsed = started.elapsed().as_nanos();
    ns_per_edge("delete_half_by_slot_then_scan", HIGH_DEG, elapsed);
}

#[test]
#[ignore = "Plan 0315: raw-block mint grow hits VectorMemory limit at 1M; deferred per implementation note"]
fn high_degree_prefix_scan_descending() {
    let bucket = seed_high_degree_hub();
    let started = Instant::now();
    let mut count: u64 = 0;
    for _ in 0..REPEAT {
        bucket.prefix_scan_descending(64, |_slot, target| {
            count = count.wrapping_add(target as u64);
        });
    }
    black_box(count);
    let elapsed = started.elapsed().as_nanos();
    ns_per_edge("prefix_scan_descending", HIGH_DEG, elapsed);
}

#[test]
#[ignore = "Plan 0315: raw-block mint grow hits VectorMemory limit at 1M; deferred per implementation note"]
fn high_degree_counterpart_resolve() {
    let bucket = seed_high_degree_hub();
    let sibling: Vec<u32> = (0..HIGH_DEG).map(|i| i.wrapping_mul(31) ^ 0xAA).collect();
    let started = Instant::now();
    let mut count: u64 = 0;
    for _ in 0..REPEAT {
        bucket.for_each_with_counterpart(&sibling, |_slot, _t, _cs, c_target| {
            count = count.wrapping_add(c_target as u64);
        });
    }
    black_box(count);
    let elapsed = started.elapsed().as_nanos();
    ns_per_edge("counterpart_resolve", HIGH_DEG, elapsed);
}

#[test]
#[ignore = "Plan 0315: raw-block mint grow hits VectorMemory limit at 1M; deferred per implementation note"]
fn high_degree_compaction() {
    let bucket = seed_high_degree_hub();
    let started = Instant::now();
    let mut survivors = Vec::with_capacity(HIGH_DEG as usize / 2);
    bucket.for_each_ascending(|slot, target| {
        if slot % 2 == 0 {
            survivors.push(target);
        }
    });
    let mut compacted = TreeCsrBucket::new(vector_memory());
    for t in survivors {
        compacted.insert(t);
    }
    black_box(compacted.stored_slots());
    let elapsed = started.elapsed().as_nanos();
    ns_per_edge("compaction", HIGH_DEG, elapsed);
}
