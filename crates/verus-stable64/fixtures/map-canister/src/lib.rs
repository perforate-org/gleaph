//! Canister fixture: `StableBTreeMap` over the verus-ic-backed `Memory` adapter.
//!
//! All Stable64 effects (including the memory manager's regions) go through
//! `VerusMemory`, so the canister has one stable-memory owner. The test proves
//! the map content survives a canister upgrade.

use ic_cdk_macros::{init, post_upgrade, query, update};
use ic_stable_structures::{
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
    StableBTreeMap,
};
use std::cell::RefCell;
use verus_stable64_memory::VerusMemory;

type Map = StableBTreeMap<u64, u64, VirtualMemory<VerusMemory>>;

thread_local! {
    static MM: RefCell<MemoryManager<VerusMemory>> =
        RefCell::new(MemoryManager::init(VerusMemory));
    static MAP: RefCell<Map> =
        RefCell::new(StableBTreeMap::init(MM.with_borrow(|mm| mm.get(MemoryId::new(0)))));
}

#[init]
fn init() {}

#[post_upgrade]
fn post_upgrade() {}

#[update]
fn put(key: u64, value: u64) {
    MAP.with_borrow_mut(|m| {
        m.insert(key, value);
    });
}

#[query]
fn get(key: u64) -> Option<u64> {
    MAP.with_borrow(|m| m.get(&key))
}

#[query]
fn len() -> u64 {
    MAP.with_borrow(|m| m.len())
}
