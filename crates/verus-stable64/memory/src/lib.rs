//! `ic-stable-structures::Memory` backed by the verus-ic Stable64 bridge.
//!
//! This is the production-owner adapter: a canister constructs its
//! `MemoryManager`/`StableBTreeMap` over [`VerusMemory`] instead of the default
//! `ic-stable-structures` backend, so all Stable64 effects go through the single
//! verus-ic bridge (the single-owner rule).
//!
//! The adapter is deliberately plain: the verified statement being adopted is
//! `verus-stable64`'s absolute-growth contract and the verus-ic Stable64 model;
//! `Memory`'s trait methods cannot thread the linear token, and claiming
//! otherwise would overstate the scope.

use ic_stable_structures::Memory;
use verus_ic::bridge::{stable64_grow, stable64_read, stable64_size, stable64_write};

/// 64 KiB-page linear memory view of the verus-ic Stable64 bridge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerusMemory;

impl Memory for VerusMemory {
    fn size(&self) -> u64 {
        stable64_size()
    }

    fn grow(&self, pages: u64) -> i64 {
        let old_pages = stable64_grow(pages);
        if old_pages == u64::MAX {
            // `ic-stable-structures` uses -1 for a failed grow, matching the IC.
            -1
        } else {
            i64::try_from(old_pages).unwrap_or(-1)
        }
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        stable64_read(dst, offset);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        stable64_write(src, offset);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::VerusMemory;
    use ic_stable_structures::{
        memory_manager::{MemoryId, MemoryManager, VirtualMemory},
        Memory, StableBTreeMap,
    };
    use std::cell::RefCell;

    type Map = StableBTreeMap<u64, u64, VirtualMemory<VerusMemory>>;

    thread_local! {
        static MM: RefCell<MemoryManager<VerusMemory>> =
            RefCell::new(MemoryManager::init(VerusMemory));
        static MAP: RefCell<Map> =
            RefCell::new(StableBTreeMap::init(MM.with_borrow(|mm| mm.get(MemoryId::new(0)))));
    }

    #[test]
    fn btreemap_round_trip_over_verus_memory() {
        verus_ic::bridge::reset_native_memory(0);
        MAP.with_borrow_mut(|m| {
            m.clear_new();
            assert!(m.is_empty());
            m.insert(7, 42);
            m.insert(9, 43);
            assert_eq!(m.get(&7), Some(42));
            assert_eq!(m.len(), 2);
            assert!(VerusMemory.size() > 0);
        });
        verus_ic::bridge::reset_native_memory(0);
    }
}
