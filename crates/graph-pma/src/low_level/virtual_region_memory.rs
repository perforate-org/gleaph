//! [`GleaphMemoryManager`] — PMA-facing analogue of [`ic_stable_structures::memory_manager::MemoryManager`]:
//! `get_bucket` / `get_extent` return [`Memory`] views scoped to one [`RegionKind`].
//!
//! ## Bucket-chain regions
//! [`VirtualBucketMemory`] maps a **single logical byte range** `[0, region.logical_len_bytes)` onto
//! the bucket chain for that kind (same idea as ic’s `MemoryManager` + virtual memories). Property
//! stores and PIDX wire their btree subregions through this path.
//!
//! ## Extent-backed regions
//! [`VirtualExtentMemory`] maps only the **head** physical extent ([`RegionManager::region_extent`]).
//! Extent growth and relocation of trailing regions are handled by [`RegionManager`]; these views
//! always read/write against the **current** layout metadata.
//!
//! ## Edge entry regions (not a single `Memory`)
//! Forward and reverse adjacency are **different** [`RegionKind`] values (and thus different
//! [`MemoryId`] slots: `1` vs `5`). Use [`GleaphMemoryManager::get_forward_edge_entries_extent`] and
//! [`GleaphMemoryManager::get_reverse_edge_entries_extent`] when you want an explicit API instead of
//! passing [`RegionKind::ForwardEdgeEntries`] / [`RegionKind::ReverseEdgeEntries`] to [`GleaphMemoryManager::get_extent`].
//!
//! [`RegionKind::ForwardEdgeEntries`] / [`RegionKind::ReverseEdgeEntries`] use **multiple physical
//! extents** (segment 0 + [`EdgeSegmentDirectory`](crate::low_level::extent::EdgeSegmentDirectory))
//! and are hydrated into [`SurfaceBaseStorage`](crate::low_level::runtime::SurfaceBaseStorage) via
//! [`crate::low_level::hydration::hydrate_edge_storage_from_stable_memory`]. There is **no** composite
//! `Memory` spanning all edge segments yet.
//!
//! ## `MemoryId` encoding
//! Use [`RegionKind::slot`] as `u8` with [`MemoryId::new`]. [`RegionKind::try_from_slot`] reverses it.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use ic_stable_structures::Memory;
use ic_stable_structures::memory_manager::MemoryId;

use super::manager::RegionManager;
use super::region::{RegionKind, RegionStorageKind, WASM_PAGE_SIZE};
use super::region_logical_slice::{read_region_logical_slice, write_region_logical_slice};
use super::{ExtentGrowthPolicy, ExtentGrowthRequest, WasmPages};

/// Errors from [`GleaphMemoryManager::get_bucket`] / [`GleaphMemoryManager::get_extent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualRegionMemoryError {
    MissingRegion(RegionKind),
    WrongStorageKind {
        kind: RegionKind,
        expected: RegionStorageKind,
        actual: RegionStorageKind,
    },
    InvalidMemoryIdSlot(u8),
}

impl fmt::Display for VirtualRegionMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRegion(k) => write!(f, "missing region definition for {:?}", k),
            Self::WrongStorageKind {
                kind,
                expected,
                actual,
            } => write!(
                f,
                "region {:?} uses {:?}, expected {:?}",
                kind, actual, expected
            ),
            Self::InvalidMemoryIdSlot(slot) => {
                write!(f, "memory id slot {slot} does not map to a RegionKind")
            }
        }
    }
}

impl std::error::Error for VirtualRegionMemoryError {}

/// Extracts the raw slot byte from [`MemoryId`].
///
/// `ic-stable-structures` 0.7 does not expose this value; `MemoryId` is a single-field newtype
/// around `u8` (see crate source). This must stay in sync with upstream layout.
#[inline]
fn memory_id_raw(id: MemoryId) -> u8 {
    debug_assert_eq!(core::mem::size_of::<MemoryId>(), 1);
    unsafe { core::mem::transmute_copy(&id) }
}

fn ensure_backing_covers(memory: &impl Memory, last_byte_exclusive: u64) -> Result<(), ()> {
    let current_pages = memory.size();
    let current_bytes = current_pages.checked_mul(WASM_PAGE_SIZE).ok_or(())?;
    if current_bytes >= last_byte_exclusive {
        return Ok(());
    }
    let missing_bytes = last_byte_exclusive - current_bytes;
    let delta_pages = missing_bytes.div_ceil(WASM_PAGE_SIZE);
    if memory.grow(delta_pages) == -1 {
        return Err(());
    }
    Ok(())
}

/// Bundles [`RegionManager`] metadata with the canister-wide backing [`Memory`], like ic's [`MemoryManager`].
pub struct GleaphMemoryManager<M: Memory> {
    manager: Rc<RefCell<RegionManager>>,
    backing: Rc<M>,
}

impl<M: Memory> Clone for GleaphMemoryManager<M> {
    fn clone(&self) -> Self {
        Self {
            manager: Rc::clone(&self.manager),
            backing: Rc::clone(&self.backing),
        }
    }
}

impl<M: Memory> GleaphMemoryManager<M> {
    pub fn new(manager: Rc<RefCell<RegionManager>>, backing: Rc<M>) -> Self {
        Self { manager, backing }
    }

    pub fn manager(&self) -> &Rc<RefCell<RegionManager>> {
        &self.manager
    }

    pub fn backing(&self) -> &Rc<M> {
        &self.backing
    }

    pub fn get_bucket(
        &self,
        kind: RegionKind,
    ) -> Result<VirtualBucketMemory<M>, VirtualRegionMemoryError> {
        let region = self
            .manager
            .borrow()
            .layout
            .region(kind)
            .ok_or(VirtualRegionMemoryError::MissingRegion(kind))?;
        if region.storage_kind() != RegionStorageKind::BucketChain {
            return Err(VirtualRegionMemoryError::WrongStorageKind {
                kind,
                expected: RegionStorageKind::BucketChain,
                actual: region.storage_kind(),
            });
        }
        Ok(VirtualBucketMemory {
            manager: Rc::clone(&self.manager),
            backing: Rc::clone(&self.backing),
            kind,
        })
    }

    pub fn get_extent(
        &self,
        kind: RegionKind,
    ) -> Result<VirtualExtentMemory<M>, VirtualRegionMemoryError> {
        let region = self
            .manager
            .borrow()
            .layout
            .region(kind)
            .ok_or(VirtualRegionMemoryError::MissingRegion(kind))?;
        if region.storage_kind() != RegionStorageKind::Extent {
            return Err(VirtualRegionMemoryError::WrongStorageKind {
                kind,
                expected: RegionStorageKind::Extent,
                actual: region.storage_kind(),
            });
        }
        Ok(VirtualExtentMemory {
            manager: Rc::clone(&self.manager),
            backing: Rc::clone(&self.backing),
            kind,
        })
    }

    /// [`RegionKind::ForwardEdgeEntries`] as an extent view (MemoryId slot `1`).
    pub fn get_forward_edge_entries_extent(
        &self,
    ) -> Result<VirtualExtentMemory<M>, VirtualRegionMemoryError> {
        self.get_extent(RegionKind::ForwardEdgeEntries)
    }

    /// [`RegionKind::ReverseEdgeEntries`] as an extent view (MemoryId slot `5`).
    pub fn get_reverse_edge_entries_extent(
        &self,
    ) -> Result<VirtualExtentMemory<M>, VirtualRegionMemoryError> {
        self.get_extent(RegionKind::ReverseEdgeEntries)
    }

    pub fn get_bucket_by_memory_id(
        &self,
        id: MemoryId,
    ) -> Result<VirtualBucketMemory<M>, VirtualRegionMemoryError> {
        let slot = memory_id_raw(id);
        let kind = RegionKind::try_from_slot(slot)
            .ok_or(VirtualRegionMemoryError::InvalidMemoryIdSlot(slot))?;
        self.get_bucket(kind)
    }

    pub fn get_extent_by_memory_id(
        &self,
        id: MemoryId,
    ) -> Result<VirtualExtentMemory<M>, VirtualRegionMemoryError> {
        let slot = memory_id_raw(id);
        let kind = RegionKind::try_from_slot(slot)
            .ok_or(VirtualRegionMemoryError::InvalidMemoryIdSlot(slot))?;
        self.get_extent(kind)
    }

    /// Reserved [`MemoryId`] for `M_v` when using a **separate** [`ic_stable_structures::memory_manager::MemoryManager`]
    /// on the canister backing store for [`ic_stable_csr`] (see `experimental-dgap`).
    #[cfg(feature = "experimental-dgap")]
    #[inline]
    pub fn dgap_vertex_memory_id() -> MemoryId {
        MemoryId::new(super::DGAP_VERTEX_MEMORY_SLOT)
    }

    /// Reserved [`MemoryId`] for `M_e` PMA `segment_edges_actual` (`M1`).
    #[cfg(feature = "experimental-dgap")]
    #[inline]
    pub fn dgap_segment_edges_actual_memory_id() -> MemoryId {
        MemoryId::new(super::DGAP_SEGMENT_EDGES_ACTUAL_MEMORY_SLOT)
    }

    /// Reserved [`MemoryId`] for `M_e` PMA `segment_edges_total` (`M2`).
    #[cfg(feature = "experimental-dgap")]
    #[inline]
    pub fn dgap_segment_edges_total_memory_id() -> MemoryId {
        MemoryId::new(super::DGAP_SEGMENT_EDGES_TOTAL_MEMORY_SLOT)
    }

    /// Reserved [`MemoryId`] for `M_e` CSR slab + log idx + log pool (`M3`).
    #[cfg(feature = "experimental-dgap")]
    #[inline]
    pub fn dgap_edges_and_log_memory_id() -> MemoryId {
        MemoryId::new(super::DGAP_EDGES_AND_LOG_MEMORY_SLOT)
    }

    /// Reserved [`MemoryId`] for `M_l` (optional append-only stream; not the per-leaf DGAP overflow pool).
    #[cfg(feature = "experimental-dgap")]
    #[inline]
    pub fn dgap_log_memory_id() -> MemoryId {
        MemoryId::new(super::DGAP_LOG_MEMORY_SLOT)
    }
}

/// [`Memory`] view over one **bucket-chain** [`RegionKind`].
pub struct VirtualBucketMemory<M: Memory> {
    manager: Rc<RefCell<RegionManager>>,
    backing: Rc<M>,
    kind: RegionKind,
}

impl<M: Memory> Clone for VirtualBucketMemory<M> {
    fn clone(&self) -> Self {
        Self {
            manager: Rc::clone(&self.manager),
            backing: Rc::clone(&self.backing),
            kind: self.kind,
        }
    }
}

impl<M: Memory> VirtualBucketMemory<M> {
    pub fn kind(&self) -> RegionKind {
        self.kind
    }
}

impl<M: Memory> Memory for VirtualBucketMemory<M> {
    fn size(&self) -> u64 {
        let mgr = self.manager.borrow();
        let Some(region) = mgr.layout.region(self.kind) else {
            return 0;
        };
        region.logical_len_bytes.div_ceil(WASM_PAGE_SIZE)
    }

    fn grow(&self, pages: u64) -> i64 {
        let old_pages = self.size();
        let Some(new_pages) = old_pages.checked_add(pages) else {
            return -1;
        };
        let new_bytes = new_pages.saturating_mul(WASM_PAGE_SIZE);

        {
            let mut mgr = self.manager.borrow_mut();
            if mgr
                .ensure_bucket_region_capacity(self.kind, new_bytes)
                .is_none()
            {
                return -1;
            }
            let Some(chain) = mgr.bucket_chain(self.kind) else {
                return -1;
            };
            let Some(header) = mgr.bucket_header(chain.tail) else {
                return -1;
            };
            let last_byte_exclusive = header.addr.0.saturating_add(mgr.bucket_size_bytes());
            drop(mgr);
            if ensure_backing_covers(self.backing.as_ref(), last_byte_exclusive).is_err() {
                return -1;
            }
        }

        {
            let mut mgr = self.manager.borrow_mut();
            if mgr.set_region_logical_len(self.kind, new_bytes).is_none() {
                return -1;
            }
        }

        let old_bytes_vm = old_pages.saturating_mul(WASM_PAGE_SIZE);
        let new_bytes_vm = new_pages.saturating_mul(WASM_PAGE_SIZE);
        if new_bytes_vm > old_bytes_vm {
            let add = usize::try_from(new_bytes_vm - old_bytes_vm).unwrap_or(0);
            let zeros = vec![0u8; add];
            let mut mgr = self.manager.borrow_mut();
            if write_region_logical_slice(
                &mut mgr,
                self.backing.as_ref(),
                self.kind,
                old_bytes_vm as usize,
                &zeros,
            )
            .is_err()
            {
                return -1;
            }
        }

        old_pages as i64
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        let offset_usize = usize::try_from(offset).expect("offset fits usize");
        let mgr = self.manager.borrow();
        let slice = read_region_logical_slice(
            &mgr,
            self.backing.as_ref(),
            self.kind,
            offset_usize,
            dst.len(),
        )
        .expect("VirtualBucketMemory::read in bounds");
        dst.copy_from_slice(&slice);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        let offset_usize = usize::try_from(offset).expect("offset fits usize");
        let mut mgr = self.manager.borrow_mut();
        write_region_logical_slice(
            &mut mgr,
            self.backing.as_ref(),
            self.kind,
            offset_usize,
            src,
        )
        .expect("VirtualBucketMemory::write");
    }
}

/// [`Memory`] view over the **head** physical extent of one extent-backed [`RegionKind`].
pub struct VirtualExtentMemory<M: Memory> {
    manager: Rc<RefCell<RegionManager>>,
    backing: Rc<M>,
    kind: RegionKind,
}

impl<M: Memory> Clone for VirtualExtentMemory<M> {
    fn clone(&self) -> Self {
        Self {
            manager: Rc::clone(&self.manager),
            backing: Rc::clone(&self.backing),
            kind: self.kind,
        }
    }
}

impl<M: Memory> VirtualExtentMemory<M> {
    pub fn kind(&self) -> RegionKind {
        self.kind
    }

    fn physical_len_bytes(&self) -> u64 {
        let mgr = self.manager.borrow();
        mgr.region_extent(self.kind)
            .map(|e| e.len_bytes)
            .unwrap_or(0)
    }
}

impl<M: Memory> Memory for VirtualExtentMemory<M> {
    fn size(&self) -> u64 {
        self.physical_len_bytes().div_ceil(WASM_PAGE_SIZE)
    }

    fn grow(&self, pages: u64) -> i64 {
        let old_pages = self.size();
        let Some(new_pages) = old_pages.checked_add(pages) else {
            return -1;
        };
        let new_bytes_vm = new_pages.saturating_mul(WASM_PAGE_SIZE);

        {
            let mut mgr = self.manager.borrow_mut();
            let extent = match mgr.region_extent(self.kind) {
                Some(e) => e,
                None => return -1,
            };
            if new_bytes_vm > extent.len_bytes {
                let shortage = new_bytes_vm - extent.len_bytes;
                let additional_pages = shortage.div_ceil(WASM_PAGE_SIZE);
                if additional_pages == 0 {
                    return -1;
                }
                let request = ExtentGrowthRequest::new(WasmPages::new(additional_pages));
                let policy = ExtentGrowthPolicy::new(
                    WasmPages::new(additional_pages.max(1)),
                    WasmPages::new(1),
                );
                let Some(decision) = mgr.plan_extent_growth(self.kind, request, policy) else {
                    return -1;
                };
                if mgr
                    .apply_extent_growth(self.kind, request, policy, decision)
                    .is_none()
                {
                    return -1;
                }
            }
            let extent = match mgr.region_extent(self.kind) {
                Some(e) => e,
                None => return -1,
            };
            let last_exclusive = extent.addr.0.saturating_add(extent.len_bytes);
            drop(mgr);
            if ensure_backing_covers(self.backing.as_ref(), last_exclusive).is_err() {
                return -1;
            }
        }

        {
            let mut mgr = self.manager.borrow_mut();
            if mgr
                .set_region_logical_len(self.kind, new_bytes_vm)
                .is_none()
            {
                return -1;
            }
        }

        let old_bytes_vm = old_pages.saturating_mul(WASM_PAGE_SIZE);
        if new_bytes_vm > old_bytes_vm {
            let add = usize::try_from(new_bytes_vm - old_bytes_vm).unwrap_or(0);
            let zeros = vec![0u8; add];
            let mut mgr = self.manager.borrow_mut();
            if write_region_logical_slice(
                &mut mgr,
                self.backing.as_ref(),
                self.kind,
                old_bytes_vm as usize,
                &zeros,
            )
            .is_err()
            {
                return -1;
            }
        }

        old_pages as i64
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        let offset_usize = usize::try_from(offset).expect("offset fits usize");
        let mgr = self.manager.borrow();
        let slice = read_region_logical_slice(
            &mgr,
            self.backing.as_ref(),
            self.kind,
            offset_usize,
            dst.len(),
        )
        .expect("VirtualExtentMemory::read in bounds");
        dst.copy_from_slice(&slice);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        let offset_usize = usize::try_from(offset).expect("offset fits usize");
        let mut mgr = self.manager.borrow_mut();
        write_region_logical_slice(
            &mut mgr,
            self.backing.as_ref(),
            self.kind,
            offset_usize,
            src,
        )
        .expect("VirtualExtentMemory::write");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::low_level::{
        BucketChain, BucketId, BucketSizeInPages, ExtentChain, ExtentId, WasmPages,
    };
    use ic_stable_structures::VectorMemory;

    fn sample_bucket_manager() -> RegionManager {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::NodePropertyStore,
            BucketChain::new(BucketId::new(1), BucketId::new(1), 0),
        );
        manager
    }

    fn sample_extent_manager() -> RegionManager {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::MaintenanceQueue,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                0,
                WasmPages::new(2),
                WasmPages::new(0),
            ),
        );
        let phys = manager
            .region_extent(RegionKind::MaintenanceQueue)
            .expect("extent")
            .len_bytes;
        manager
            .set_region_logical_len(RegionKind::MaintenanceQueue, phys)
            .expect("logical len");
        manager
    }

    fn sample_forward_reverse_edge_extent_manager() -> RegionManager {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        for kind in [
            RegionKind::ForwardEdgeEntries,
            RegionKind::ReverseEdgeEntries,
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::new(1),
                    ExtentId::new(1),
                    0,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            let phys = manager.region_extent(kind).expect("extent").len_bytes;
            manager
                .set_region_logical_len(kind, phys)
                .expect("logical len");
        }
        manager
    }

    #[test]
    fn region_kind_slot_roundtrips_memory_id() {
        let kinds = [
            RegionKind::ForwardVertexTable,
            RegionKind::NodePropertyStore,
            RegionKind::ShardCanisterDirectory,
        ];
        for k in kinds {
            let slot = k.slot() as u8;
            let id = MemoryId::new(slot);
            assert_eq!(memory_id_raw(id), slot);
            assert_eq!(RegionKind::try_from_slot(slot), Some(k));
        }
    }

    #[test]
    fn get_bucket_wrong_kind_errors() {
        let manager = Rc::new(RefCell::new(sample_extent_manager()));
        let mem = Rc::new(VectorMemory::default());
        let hub = GleaphMemoryManager::new(manager, mem);
        match hub.get_bucket(RegionKind::MaintenanceQueue) {
            Err(VirtualRegionMemoryError::WrongStorageKind { .. }) => {}
            Ok(_) => panic!("expected WrongStorageKind, got Ok"),
            Err(e) => panic!("expected WrongStorageKind, got {e}"),
        }
    }

    #[test]
    fn get_extent_wrong_kind_errors() {
        let manager = Rc::new(RefCell::new(sample_bucket_manager()));
        let mem = Rc::new(VectorMemory::default());
        let hub = GleaphMemoryManager::new(manager, mem);
        match hub.get_extent(RegionKind::NodePropertyStore) {
            Err(VirtualRegionMemoryError::WrongStorageKind { .. }) => {}
            Ok(_) => panic!("expected WrongStorageKind, got Ok"),
            Err(e) => panic!("expected WrongStorageKind, got {e}"),
        }
    }

    #[test]
    fn virtual_bucket_read_write_roundtrip() {
        let manager = Rc::new(RefCell::new(sample_bucket_manager()));
        let mem = Rc::new(VectorMemory::default());
        let hub = GleaphMemoryManager::new(Rc::clone(&manager), Rc::clone(&mem));
        let v = hub.get_bucket(RegionKind::NodePropertyStore).unwrap();

        v.write(0, &[1, 2, 3, 4]);
        let mut buf = [0u8; 4];
        v.read(0, &mut buf);
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn virtual_bucket_grow_extends_logical_region() {
        let manager = Rc::new(RefCell::new(sample_bucket_manager()));
        let mem = Rc::new(VectorMemory::default());
        let hub = GleaphMemoryManager::new(Rc::clone(&manager), mem);
        let v = hub.get_bucket(RegionKind::NodePropertyStore).unwrap();

        assert_eq!(v.size(), 0);
        assert_eq!(v.grow(1), 0);
        assert_eq!(v.size(), 1);
        let mut buf = vec![0u8; WASM_PAGE_SIZE as usize];
        v.read(0, &mut buf);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn virtual_extent_read_write_and_grow() {
        let manager = Rc::new(RefCell::new(sample_extent_manager()));
        let mem = Rc::new(VectorMemory::default());
        let hub = GleaphMemoryManager::new(Rc::clone(&manager), Rc::clone(&mem));
        let v = hub.get_extent(RegionKind::MaintenanceQueue).unwrap();

        assert!(v.size() >= 2);
        v.write(0, &[9, 8, 7]);
        let mut buf = [0u8; 3];
        v.read(0, &mut buf);
        assert_eq!(buf, [9, 8, 7]);

        let old_pages = v.size();
        assert_eq!(v.grow(1), old_pages as i64);
        assert_eq!(v.size(), old_pages + 1);
    }

    #[test]
    fn get_bucket_by_invalid_memory_id_slot_errors() {
        let manager = Rc::new(RefCell::new(sample_bucket_manager()));
        let mem = Rc::new(VectorMemory::default());
        let hub = GleaphMemoryManager::new(manager, mem);
        let id = MemoryId::new(99);
        match hub.get_bucket_by_memory_id(id) {
            Err(VirtualRegionMemoryError::InvalidMemoryIdSlot(99)) => {}
            Ok(_) => panic!("expected InvalidMemoryIdSlot, got Ok"),
            Err(e) => panic!("expected InvalidMemoryIdSlot(99), got {e}"),
        }
    }

    #[test]
    fn get_bucket_by_memory_id_matches_kind() {
        let manager = Rc::new(RefCell::new(sample_bucket_manager()));
        let mem = Rc::new(VectorMemory::default());
        let hub = GleaphMemoryManager::new(manager, mem);
        let id = MemoryId::new(RegionKind::NodePropertyStore.slot() as u8);
        let v = hub.get_bucket_by_memory_id(id).unwrap();
        v.write(10, &[0xab]);
        let mut b = [0u8; 1];
        v.read(10, &mut b);
        assert_eq!(b[0], 0xab);
    }

    #[test]
    fn forward_and_reverse_edge_entry_extents_are_distinct_regions() {
        let manager = Rc::new(RefCell::new(sample_forward_reverse_edge_extent_manager()));
        let mem = Rc::new(VectorMemory::default());
        let hub = GleaphMemoryManager::new(Rc::clone(&manager), Rc::clone(&mem));

        let fwd = hub.get_forward_edge_entries_extent().unwrap();
        let rev = hub.get_reverse_edge_entries_extent().unwrap();
        fwd.write(0, &[0xf1]);
        rev.write(0, &[0xe5]);
        let mut a = [0u8; 1];
        let mut b = [0u8; 1];
        fwd.read(0, &mut a);
        rev.read(0, &mut b);
        assert_eq!(a[0], 0xf1);
        assert_eq!(b[0], 0xe5);
    }
}
