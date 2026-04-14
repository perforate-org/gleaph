//! `ic-stable-csr`-backed adjacency storage for `graph-store`.
//!
//! Phase 1 of the `graph-store` migration keeps property/index storage on the
//! legacy low-level stack, but moves adjacency to fixed-slot
//! [`MemoryManager`](ic_stable_structures::memory_manager::MemoryManager)
//! regions. This module defines the canonical dense vertex table and a thin wrapper over
//! [`ic_stable_csr::csr::CsrGraphWithGcQueueSparseDeleted`].

use ic_stable_csr::csr::{CsrGraphError, CsrGraphWithGcQueueSparseDeleted};
use ic_stable_csr::dgap::SegmentMaintainThresholds;
use ic_stable_structures::storable::Bound;
use ic_stable_structures::Memory;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{StableCell, Storable};
use std::borrow::Cow;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;

use crate::low_level::{
    EdgeEntry, EdgeLogicalLocatorSidecar, GraphInsertPolicy, ShardCanisterDirectory,
    ShardDirectoryStore, VertexEntry,
};

pub const GRAPH_STORE_MEMORY_ID_FORWARD_VERTEX_TABLE: MemoryId = MemoryId::new(0);
pub const GRAPH_STORE_MEMORY_ID_REVERSE_VERTEX_TABLE: MemoryId = MemoryId::new(1);
pub const GRAPH_STORE_MEMORY_ID_FORWARD_SEGMENT_EDGE_COUNTS: MemoryId = MemoryId::new(2);
pub const GRAPH_STORE_MEMORY_ID_FORWARD_EDGES_AND_LOG: MemoryId = MemoryId::new(3);
pub const GRAPH_STORE_MEMORY_ID_REVERSE_SEGMENT_EDGE_COUNTS: MemoryId = MemoryId::new(4);
pub const GRAPH_STORE_MEMORY_ID_REVERSE_EDGES_AND_LOG: MemoryId = MemoryId::new(5);
pub const GRAPH_STORE_MEMORY_ID_DELETED_VERTICES: MemoryId = MemoryId::new(6);
pub const GRAPH_STORE_MEMORY_ID_ADJACENCY_GC_QUEUE: MemoryId = MemoryId::new(7);
pub const GRAPH_STORE_MEMORY_ID_NODE_PROPERTY_STORE: MemoryId = MemoryId::new(8);
pub const GRAPH_STORE_MEMORY_ID_EDGE_PROPERTY_STORE: MemoryId = MemoryId::new(9);
pub const GRAPH_STORE_MEMORY_ID_PROPERTY_INDEX: MemoryId = MemoryId::new(10);
pub const GRAPH_STORE_MEMORY_ID_LABEL_CATALOG: MemoryId = MemoryId::new(11);
pub const GRAPH_STORE_MEMORY_ID_GC_STATE: MemoryId = MemoryId::new(12);
pub const GRAPH_STORE_MEMORY_ID_SHARD_CANISTER_DIRECTORY: MemoryId = MemoryId::new(13);
/// Serialized maintenance queue (`MGQ1` header + items) in the reserved graph root map.
pub const GRAPH_STORE_MEMORY_ID_MAINTENANCE_QUEUE: MemoryId = MemoryId::new(14);
/// Disjoint forward-ordinal dirty intervals for incremental maintenance (`StableBTreeMap`).
pub const GRAPH_STORE_MEMORY_ID_MAINTENANCE_DIRTY_ORDINALS: MemoryId = MemoryId::new(15);
pub const GRAPH_STORE_RESERVED_ROOT_PAGE_START: u64 = 1024;
/// Upper bound (exclusive) on pages reserved for the graph-root [`MemoryManager`] virtual slots.
/// Must stay large enough for all fixed ids (including btree growth) without `grow failed`.
pub const GRAPH_STORE_RESERVED_ROOT_PAGE_END: u64 = 3072;
const WASM_PAGE_BYTES: u64 = 65_536;

/// Canonical fixed-slot layout for the phase-1 `graph-store` stable memory map.
///
/// Canonical fixed-slot ids for the graph root `MemoryManager` (adjacency, properties, PIDX, …).
pub const GRAPH_STORE_FIXED_MEMORY_IDS: [MemoryId; 16] = [
    GRAPH_STORE_MEMORY_ID_FORWARD_VERTEX_TABLE,
    GRAPH_STORE_MEMORY_ID_REVERSE_VERTEX_TABLE,
    GRAPH_STORE_MEMORY_ID_FORWARD_SEGMENT_EDGE_COUNTS,
    GRAPH_STORE_MEMORY_ID_FORWARD_EDGES_AND_LOG,
    GRAPH_STORE_MEMORY_ID_REVERSE_SEGMENT_EDGE_COUNTS,
    GRAPH_STORE_MEMORY_ID_REVERSE_EDGES_AND_LOG,
    GRAPH_STORE_MEMORY_ID_DELETED_VERTICES,
    GRAPH_STORE_MEMORY_ID_ADJACENCY_GC_QUEUE,
    GRAPH_STORE_MEMORY_ID_NODE_PROPERTY_STORE,
    GRAPH_STORE_MEMORY_ID_EDGE_PROPERTY_STORE,
    GRAPH_STORE_MEMORY_ID_PROPERTY_INDEX,
    GRAPH_STORE_MEMORY_ID_LABEL_CATALOG,
    GRAPH_STORE_MEMORY_ID_GC_STATE,
    GRAPH_STORE_MEMORY_ID_SHARD_CANISTER_DIRECTORY,
    GRAPH_STORE_MEMORY_ID_MAINTENANCE_QUEUE,
    GRAPH_STORE_MEMORY_ID_MAINTENANCE_DIRTY_ORDINALS,
];

pub type GraphAdjacencyMemory<M> = VirtualMemory<M>;
pub type GraphAdjacencyBackend<M> = CsrGraphWithGcQueueSparseDeleted<
    VertexEntry,
    EdgeEntry,
    GraphAdjacencyMemory<M>,
    GraphAdjacencyMemory<M>,
    GraphAdjacencyMemory<M>,
    GraphAdjacencyMemory<M>,
    GraphAdjacencyMemory<M>,
    GraphAdjacencyMemory<M>,
    GraphAdjacencyMemory<M>,
>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StableBytes(Vec<u8>);

impl Storable for StableBytes {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }

    const BOUND: Bound = Bound::Unbounded;
}

#[derive(Debug)]
pub enum GraphStoreSlotError {
    StableCellInit(&'static str),
    StableCellWrite(&'static str),
    InvalidShardCanisterDirectory(&'static str),
}

impl fmt::Display for GraphStoreSlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StableCellInit(msg) => write!(f, "stable cell init failed: {msg}"),
            Self::StableCellWrite(msg) => write!(f, "stable cell write failed: {msg}"),
            Self::InvalidShardCanisterDirectory(msg) => {
                write!(f, "invalid shard canister directory: {msg}")
            }
        }
    }
}

impl std::error::Error for GraphStoreSlotError {}

/// Phase-1 adjacency state for `graph-store`.
///
/// This keeps the `ic-stable-csr` graph, the semantic locator sidecar, and
/// the existing graph insert policy together so the facade can migrate off the
/// hydrated PMA runtime without changing higher-level graph semantics in one
/// step.
pub struct GraphAdjacency<M: Memory + Clone> {
    memory_slots: GraphStoreMemorySlots<M>,
    graph: GraphAdjacencyBackend<M>,
    logical_locators: EdgeLogicalLocatorSidecar,
    insert_policy: GraphInsertPolicy,
}

/// Canonical fixed-slot accessor over one canister/root memory.
///
/// Phase 1 uses this for adjacency only, but the property/index migration can
/// move onto the same accessor without re-defining slot ownership again.
pub struct GraphStoreMemorySlots<M: Memory + Clone> {
    memory_manager: MemoryManager<M>,
}

/// Borrowed memory handle that forwards the stable-memory interface without
/// cloning or taking ownership of the underlying backing store.
pub struct BorrowedMemory<'a, M: Memory>(&'a M);

impl<'a, M: Memory> Clone for BorrowedMemory<'a, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, M: Memory> Copy for BorrowedMemory<'a, M> {}

impl<'a, M: Memory> BorrowedMemory<'a, M> {
    pub fn new(memory: &'a M) -> Self {
        Self(memory)
    }
}

impl<'a, M: Memory> Memory for BorrowedMemory<'a, M> {
    fn size(&self) -> u64 {
        self.0.size()
    }

    fn grow(&self, pages: u64) -> i64 {
        self.0.grow(pages)
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        self.0.read(offset, dst)
    }

    fn write(&self, offset: u64, src: &[u8]) {
        self.0.write(offset, src)
    }
}

/// [`Rc`] handle to the graph root stable memory, implementing [`Memory`] for use inside
/// [`GraphStoreMemorySlots`] without cloning the full backing store on every accessor.
#[derive(Clone)]
pub struct RcGraphMemory<M: Memory>(pub Rc<M>);

impl<M: Memory> Memory for RcGraphMemory<M> {
    fn size(&self) -> u64 {
        self.0.size()
    }

    fn grow(&self, pages: u64) -> i64 {
        self.0.grow(pages)
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        self.0.read(offset, dst)
    }

    fn write(&self, offset: u64, src: &[u8]) {
        self.0.write(offset, src)
    }
}

/// Fixed page-range view used when graph-store needs to coexist with the
/// legacy region-manager layout in the same root stable memory.
#[derive(Clone)]
pub struct PageRangeMemory<M: Memory + Clone> {
    memory: M,
    page_range: Range<u64>,
}

impl<M: Memory + Clone> PageRangeMemory<M> {
    pub fn new(memory: M, page_range: Range<u64>) -> Self {
        Self { memory, page_range }
    }
}

impl<M: Memory + Clone> Memory for PageRangeMemory<M> {
    fn size(&self) -> u64 {
        let base_size = self.memory.size();
        if base_size < self.page_range.start {
            0
        } else if base_size > self.page_range.end {
            self.page_range.end - self.page_range.start
        } else {
            base_size - self.page_range.start
        }
    }

    fn grow(&self, pages: u64) -> i64 {
        let base_size = self.memory.size();
        if base_size < self.page_range.start {
            self.memory
                .grow(self.page_range.start - base_size + pages)
                .min(0)
        } else if base_size >= self.page_range.end {
            if pages == 0 {
                (self.page_range.end - self.page_range.start) as i64
            } else {
                -1
            }
        } else {
            let pages_left = self.page_range.end - base_size;
            if pages_left < pages {
                -1
            } else {
                let r = self.memory.grow(pages);
                if r < 0 {
                    r
                } else {
                    r - self.page_range.start as i64
                }
            }
        }
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        self.memory.read(
            self.page_range.start * WASM_PAGE_BYTES + offset,
            dst,
        )
    }

    unsafe fn read_unsafe(&self, offset: u64, dst: *mut u8, count: usize) {
        unsafe {
            self.memory
                .read_unsafe(self.page_range.start * WASM_PAGE_BYTES + offset, dst, count)
        }
    }

    fn write(&self, offset: u64, src: &[u8]) {
        self.memory.write(
            self.page_range.start * WASM_PAGE_BYTES + offset,
            src,
        )
    }
}

impl<M: Memory + Clone> GraphStoreMemorySlots<M> {
    pub fn new(memory: M) -> Self {
        Self {
            memory_manager: MemoryManager::init(memory),
        }
    }

    pub fn for_root_memory(memory: M) -> GraphStoreMemorySlots<PageRangeMemory<M>> {
        let current_pages = memory.size();
        if current_pages < GRAPH_STORE_RESERVED_ROOT_PAGE_END {
            let _ = memory.grow(GRAPH_STORE_RESERVED_ROOT_PAGE_END - current_pages);
        }
        GraphStoreMemorySlots::new(PageRangeMemory::new(
            memory,
            GRAPH_STORE_RESERVED_ROOT_PAGE_START..GRAPH_STORE_RESERVED_ROOT_PAGE_END,
        ))
    }

    pub fn memory_manager(&self) -> &MemoryManager<M> {
        &self.memory_manager
    }

    pub fn forward_vertex_table(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_FORWARD_VERTEX_TABLE)
    }

    pub fn reverse_vertex_table(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_REVERSE_VERTEX_TABLE)
    }

    pub fn forward_segment_edge_counts(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_FORWARD_SEGMENT_EDGE_COUNTS)
    }

    pub fn forward_edges_and_log(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_FORWARD_EDGES_AND_LOG)
    }

    pub fn reverse_segment_edge_counts(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_REVERSE_SEGMENT_EDGE_COUNTS)
    }

    pub fn reverse_edges_and_log(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_REVERSE_EDGES_AND_LOG)
    }

    pub fn deleted_vertices(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager.get(GRAPH_STORE_MEMORY_ID_DELETED_VERTICES)
    }

    pub fn adjacency_gc_queue(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_ADJACENCY_GC_QUEUE)
    }

    pub fn node_property_store(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_NODE_PROPERTY_STORE)
    }

    pub fn edge_property_store(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_EDGE_PROPERTY_STORE)
    }

    pub fn property_index(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager.get(GRAPH_STORE_MEMORY_ID_PROPERTY_INDEX)
    }

    pub fn label_catalog(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager.get(GRAPH_STORE_MEMORY_ID_LABEL_CATALOG)
    }

    pub fn gc_state(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager.get(GRAPH_STORE_MEMORY_ID_GC_STATE)
    }

    pub fn shard_canister_directory(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_SHARD_CANISTER_DIRECTORY)
    }

    pub fn maintenance_queue(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_MAINTENANCE_QUEUE)
    }

    pub fn maintenance_dirty_ordinals(&self) -> GraphAdjacencyMemory<M> {
        self.memory_manager
            .get(GRAPH_STORE_MEMORY_ID_MAINTENANCE_DIRTY_ORDINALS)
    }

    /// Reads one slot as an opaque stable blob.
    pub fn load_blob(&self, id: MemoryId) -> Vec<u8> {
        let cell: StableCell<StableBytes, GraphAdjacencyMemory<M>> =
            StableCell::init(self.memory_manager.get(id), StableBytes::default());
        cell.get().0.clone()
    }

    /// Stores one opaque stable blob into a fixed slot.
    pub fn store_blob(&self, id: MemoryId, bytes: Vec<u8>) {
        let mut cell: StableCell<StableBytes, GraphAdjacencyMemory<M>> =
            StableCell::init(self.memory_manager.get(id), StableBytes::default());
        let _prev = cell.set(StableBytes(bytes));
    }

    /// Reads the phase-2 shard canister directory payload directly from its
    /// fixed slot, independent of the legacy region-manager path.
    pub fn load_shard_canister_directory(&self) -> Result<ShardCanisterDirectory, GraphStoreSlotError> {
        Ok(ShardDirectoryStore::open(self).to_directory())
    }

    /// Persists the shard canister directory payload directly into its fixed
    /// slot, independent of the legacy region-manager path.
    pub fn store_shard_canister_directory(
        &self,
        directory: &ShardCanisterDirectory,
    ) -> Result<(), GraphStoreSlotError> {
        let mut store = ShardDirectoryStore::open(self);
        store.replace_from_directory(directory);
        Ok(())
    }
}

impl<M: Memory + Clone> GraphAdjacency<M> {
    #[allow(clippy::too_many_arguments)]
    pub fn format_new(
        memory: M,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
        maintain_thresholds: Option<SegmentMaintainThresholds>,
    ) -> Result<Self, CsrGraphError> {
        let memory_slots = GraphStoreMemorySlots::new(memory);
        let graph = GraphAdjacencyBackend::format_new_with_gc_queue(
            memory_slots.forward_vertex_table(),
            memory_slots.reverse_vertex_table(),
            memory_slots.forward_segment_edge_counts(),
            memory_slots.forward_edges_and_log(),
            memory_slots.reverse_segment_edge_counts(),
            memory_slots.reverse_edges_and_log(),
            memory_slots.deleted_vertices(),
            memory_slots.adjacency_gc_queue(),
            elem_capacity,
            segment_count,
            segment_size,
            num_edges,
            maintain_thresholds,
        )?;
        Ok(Self {
            memory_slots,
            graph,
            logical_locators: EdgeLogicalLocatorSidecar::default(),
            insert_policy: GraphInsertPolicy::default(),
        })
    }

    pub fn open_existing(
        memory: M,
        maintain_thresholds: Option<SegmentMaintainThresholds>,
    ) -> Result<Self, CsrGraphError> {
        let memory_slots = GraphStoreMemorySlots::new(memory);
        let graph = GraphAdjacencyBackend::open_existing_with_gc_queue(
            memory_slots.forward_vertex_table(),
            memory_slots.reverse_vertex_table(),
            memory_slots.forward_segment_edge_counts(),
            memory_slots.forward_edges_and_log(),
            memory_slots.reverse_segment_edge_counts(),
            memory_slots.reverse_edges_and_log(),
            memory_slots.deleted_vertices(),
            memory_slots.adjacency_gc_queue(),
            maintain_thresholds,
        )?;
        Ok(Self {
            memory_slots,
            graph,
            logical_locators: EdgeLogicalLocatorSidecar::default(),
            insert_policy: GraphInsertPolicy::default(),
        })
    }

    pub fn memory_slots(&self) -> &GraphStoreMemorySlots<M> {
        &self.memory_slots
    }

    pub fn memory_manager(&self) -> &MemoryManager<M> {
        self.memory_slots.memory_manager()
    }

    pub fn graph(&self) -> &GraphAdjacencyBackend<M> {
        &self.graph
    }

    pub fn logical_locators(&self) -> &EdgeLogicalLocatorSidecar {
        &self.logical_locators
    }

    pub fn logical_locators_mut(&mut self) -> &mut EdgeLogicalLocatorSidecar {
        &mut self.logical_locators
    }

    pub fn insert_policy(&self) -> &GraphInsertPolicy {
        &self.insert_policy
    }

    pub fn insert_policy_mut(&mut self) -> &mut GraphInsertPolicy {
        &mut self.insert_policy
    }
}

#[inline]
pub const fn graph_store_fixed_memory_ids() -> [MemoryId; 16] {
    GRAPH_STORE_FIXED_MEMORY_IDS
}

#[cfg(test)]
mod tests {
    use super::GraphStoreMemorySlots;
    use crate::low_level::{ShardCanisterDirectory, ShardDirectoryStore};
    use candid::Principal;
    use ic_stable_structures::VectorMemory;

    #[test]
    fn fixed_slot_shard_directory_round_trips_independently() {
        let slots = GraphStoreMemorySlots::new(VectorMemory::default());
        let mut dir = ShardCanisterDirectory::default();
        assert_eq!(
            dir.push_principal(Principal::from_slice(&[1, 2, 3, 4]), false),
            Some(0)
        );
        assert_eq!(
            dir.push_principal(Principal::from_slice(&[9, 8, 7, 6, 5]), false),
            Some(1)
        );

        slots
            .store_shard_canister_directory(&dir)
            .expect("store shard canister directory");
        let loaded = slots
            .load_shard_canister_directory()
            .expect("load shard canister directory");
        assert_eq!(loaded, dir);
        let reopened = ShardDirectoryStore::open(&slots);
        assert_eq!(reopened.principal(0), dir.principal(0));
        assert_eq!(reopened.principal(1), dir.principal(1));
    }
}
