//! [`ic_stable_structures::Memory`] view over btree payload bytes `[PROP_STORE_V1_HEADER_LEN..]` for
//! [`RegionKind::NodePropertyStore`] / [`RegionKind::EdgePropertyStore`].
//!
//! Reads and writes go through `region_memory`, a logical [`Memory`] for the whole region (typically
//! [`crate::low_level::VirtualBucketMemory`]) so bucket-chain I/O stays consistent with
//! [`crate::low_level::GleaphMemoryManager`].

use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_structures::Memory as IcMemory;

use crate::low_level::{RegionKind, RegionManager, WASM_PAGE_SIZE};
use ic_stable_structures::Memory as StableMemoryTrait;

use super::pstore_v1_layout::PROP_STORE_V1_HEADER_LEN;

const IC_PAGE: u64 = WASM_PAGE_SIZE;

#[derive(Clone)]
pub struct PropertyStoreBtreeSubregionIcMemory<R: StableMemoryTrait> {
    manager: Rc<RefCell<RegionManager>>,
    region_memory: R,
    btree_content_len: Rc<RefCell<u64>>,
    region_kind: RegionKind,
}

impl<R: StableMemoryTrait> PropertyStoreBtreeSubregionIcMemory<R> {
    pub fn new(
        manager: Rc<RefCell<RegionManager>>,
        region_memory: R,
        btree_content_len: Rc<RefCell<u64>>,
        region_kind: RegionKind,
    ) -> Self {
        Self {
            manager,
            region_memory,
            btree_content_len,
            region_kind,
        }
    }

    pub fn btree_payload_byte_len_rc(&self) -> Rc<RefCell<u64>> {
        Rc::clone(&self.btree_content_len)
    }

    fn base() -> u64 {
        PROP_STORE_V1_HEADER_LEN as u64
    }
}

impl<R: StableMemoryTrait> IcMemory for PropertyStoreBtreeSubregionIcMemory<R> {
    fn size(&self) -> u64 {
        (*self.btree_content_len.borrow()).div_ceil(IC_PAGE)
    }

    fn grow(&self, pages: u64) -> i64 {
        let old_pages = self.size();
        let Some(new_pages) = old_pages.checked_add(pages) else {
            return -1;
        };
        let new_content_len = new_pages.saturating_mul(IC_PAGE);
        *self.btree_content_len.borrow_mut() = new_content_len;
        let total = Self::base().saturating_add(new_content_len);

        let cur_vm_pages = self.region_memory.size();
        let need_pages = total.div_ceil(IC_PAGE);
        let delta = need_pages.saturating_sub(cur_vm_pages);
        if delta > 0 && self.region_memory.grow(delta) == -1 {
            return -1;
        }

        {
            let mut mgr = self.manager.borrow_mut();
            let _ = mgr.set_region_logical_len(self.region_kind, total);
        }

        let prior = old_pages.saturating_mul(IC_PAGE);
        if new_content_len > prior {
            let start = Self::base().saturating_add(prior);
            let add = (new_content_len - prior) as usize;
            let zeros = vec![0u8; add];
            self.region_memory.write(start, &zeros);
        }
        old_pages as i64
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        let abs = Self::base().saturating_add(offset);
        self.region_memory.read(abs, dst);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        let end = offset
            .checked_add(src.len() as u64)
            .expect("property store btree write");
        let mut cur = *self.btree_content_len.borrow();
        if end > cur {
            cur = end;
        }
        // `Memory::size()` is in Wasm pages; ic-stable-structures' virtual byte capacity is
        // `size() * WASM_PAGE_SIZE` = ceil(cur, page) * page. The region logical length must
        // cover that full virtual span or btree reads land in "unmapped" zeros (Bad magic).
        cur = cur.div_ceil(IC_PAGE).saturating_mul(IC_PAGE);
        *self.btree_content_len.borrow_mut() = cur;
        let abs = Self::base().saturating_add(offset);
        self.region_memory.write(abs, src);
        let mut mgr = self.manager.borrow_mut();
        let _ = mgr.set_region_logical_len(self.region_kind, Self::base().saturating_add(cur));
    }
}
