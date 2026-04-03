//! [`ic_stable_structures::Memory`] view over the btree payload slice (PIDX bytes `[PIDX_V3_HEADER_LEN..]`).

use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_structures::Memory as IcMemory;

use crate::low_level::{
    ExtentGrowthPolicy, ExtentGrowthRequest, RegionKind, RegionManager, RegionStorageKind,
    WASM_PAGE_SIZE, WasmPages,
};
use crate::stable::Memory as StableMemoryTrait;

use super::pidx_v3_layout::PIDX_V3_HEADER_LEN;
use super::storage::region_io::{
    read_property_index_region_slice, write_property_index_region_logical_slice,
};

const IC_PAGE: u64 = WASM_PAGE_SIZE;

/// Backing btree bytes live in the PIDX region after the v3 fixed header.
#[derive(Clone)]
pub struct PropertyIndexBtreeSubregionIcMemory<M: StableMemoryTrait> {
    manager: Rc<RefCell<RegionManager>>,
    memory: Rc<RefCell<M>>,
    btree_content_len: Rc<RefCell<u64>>,
}

impl<M: StableMemoryTrait> PropertyIndexBtreeSubregionIcMemory<M> {
    pub fn new(
        manager: Rc<RefCell<RegionManager>>,
        memory: Rc<RefCell<M>>,
        btree_content_len: Rc<RefCell<u64>>,
    ) -> Self {
        Self {
            manager,
            memory,
            btree_content_len,
        }
    }

    pub fn btree_payload_byte_len_rc(&self) -> Rc<RefCell<u64>> {
        Rc::clone(&self.btree_content_len)
    }

    fn base() -> u64 {
        PIDX_V3_HEADER_LEN as u64
    }
}

impl<M: StableMemoryTrait> IcMemory for PropertyIndexBtreeSubregionIcMemory<M> {
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
        {
            let mut mgr = self.manager.borrow_mut();
            let Some(region) = mgr.layout.region(RegionKind::PropertyIndex) else {
                return -1;
            };
            match region.storage_kind() {
                RegionStorageKind::BucketChain => {
                    let _ = mgr.ensure_bucket_region_capacity(RegionKind::PropertyIndex, total);
                }
                RegionStorageKind::Extent => {
                    let extent = match mgr.region_extent(RegionKind::PropertyIndex) {
                        Some(e) => e,
                        None => return -1,
                    };
                    if total > extent.len_bytes {
                        let shortage = total - extent.len_bytes;
                        let additional_pages = shortage.div_ceil(WASM_PAGE_SIZE);
                        if additional_pages == 0 {
                            return -1;
                        }
                        let request = ExtentGrowthRequest::new(WasmPages::new(additional_pages));
                        let policy = ExtentGrowthPolicy::new(
                            WasmPages::new(additional_pages.max(1)),
                            WasmPages::new(1),
                        );
                        let Some(decision) =
                            mgr.plan_extent_growth(RegionKind::PropertyIndex, request, policy)
                        else {
                            return -1;
                        };
                        if mgr
                            .apply_extent_growth(
                                RegionKind::PropertyIndex,
                                request,
                                policy,
                                decision,
                            )
                            .is_none()
                        {
                            return -1;
                        }
                    }
                }
            }
            let _ = mgr.set_region_logical_len(RegionKind::PropertyIndex, total);
        }
        let prior = old_pages.saturating_mul(IC_PAGE);
        if new_content_len > prior {
            let start = (Self::base() + prior) as usize;
            let add = (new_content_len - prior) as usize;
            let zeros = vec![0u8; add];
            let mut mgr = self.manager.borrow_mut();
            let m = self.memory.borrow();
            if write_property_index_region_logical_slice(&mut mgr, &*m, start, &zeros).is_err() {
                return -1;
            }
        }
        old_pages as i64
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        let abs = Self::base().saturating_add(offset);
        let abs_usize = usize::try_from(abs).expect("offset");
        let mgr = self.manager.borrow();
        let m = self.memory.borrow();
        let got = read_property_index_region_slice(&mgr, &*m, abs_usize, dst.len())
            .expect("property index btree read");
        dst.copy_from_slice(&got);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        let end = offset
            .checked_add(src.len() as u64)
            .expect("property index btree write");
        let mut cur = *self.btree_content_len.borrow();
        if end > cur {
            cur = end;
        }
        cur = cur.div_ceil(IC_PAGE).saturating_mul(IC_PAGE);
        *self.btree_content_len.borrow_mut() = cur;
        let abs = Self::base().saturating_add(offset) as usize;
        let mut mgr = self.manager.borrow_mut();
        let m = self.memory.borrow();
        write_property_index_region_logical_slice(&mut mgr, &*m, abs, src)
            .expect("property index btree write");
        let _ =
            mgr.set_region_logical_len(RegionKind::PropertyIndex, Self::base().saturating_add(cur));
    }
}

#[cfg(test)]
mod tests {
    use super::IC_PAGE;

    #[test]
    fn ic_page_matches_graph_pma_wasm_page() {
        assert_eq!(IC_PAGE, crate::low_level::WASM_PAGE_SIZE);
    }
}
