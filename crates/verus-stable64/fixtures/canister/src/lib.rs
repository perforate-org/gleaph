//! Minimal canister fixture that owns its Stable64 memory through `verus-stable64`.
//!
//! It exists to exercise the leaf end to end on wasm/PocketIC: absolute-target
//! growth, stable writes/reads, and persistence across a canister upgrade.

use candid::CandidType;
use ic_cdk_macros::{init, post_upgrade, query, update};
use verus_ic::bridge::{stable64_read, stable64_size, stable64_write};

#[init]
fn init() {}

#[post_upgrade]
fn post_upgrade() {}

/// Grow to at least `target_pages` through the verified leaf's plain mirror and
/// return the resulting page count.
#[update]
fn grow_to_pages(target_pages: u64) -> u64 {
    verus_stable64::grow_to(target_pages)
}

/// Write a marker at stable offset 0, growing to one page first when empty.
#[update]
fn write_marker(data: Vec<u8>) {
    if stable64_size() == 0 {
        verus_stable64::grow_to(1);
    }
    stable64_write(&data, 0);
}

/// Read the first eight bytes of stable memory (empty before the first grow).
#[query]
fn read_marker() -> Vec<u8> {
    if stable64_size() == 0 {
        return Vec::new();
    }
    let mut buf = [0u8; 8];
    stable64_read(&mut buf, 0);
    buf.to_vec()
}

/// Current Stable64 page count.
#[query]
fn pages() -> u64 {
    stable64_size()
}

// Keep the Candid type import used so `candid` stays an explicit dependency for
// generated interface consumers.
#[allow(dead_code)]
fn _candid_marker<T: CandidType>() {}
