//! Facade over the low-level `graph-store` runtime (`GraphRuntime` + regions).
//!
//! This module deliberately stays small. It does not hide the low-level model;
//! it only bundles the pieces that most callers would otherwise wire together
//! by hand:
//!
//! - region-manager metadata
//! - hydrated forward/reverse graph runtime state
//! - stable-memory hydration and writeback entrypoints

mod adapter_ops;
mod errors;
mod facade_types;
mod lifecycle_ops;
mod property_ops;
mod store_traits;

#[cfg(test)]
use std::cell::Cell;
use std::cell::{Ref, RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Instant;

use crate::VecMemory;
use crate::adjacency::{BorrowedMemory, GraphStoreMemorySlots, PageRangeMemory, RcGraphMemory};
use gleaph_gql::Value;
use gleaph_graph_kernel::{EdgeId, LabelId, NodeId, PropertyMap};
use ic_stable_structures::Memory;

use crate::low_level::{
    BucketSizeInPages, EdgeEntry, EdgeReplaceSpec, EdgeTombstoneSpec, ExtentChain,
    ExtentGrowthPolicy, ExtentGrowthRequest, ExtentId, ForwardSurfaceRuntime,
    GraphBatchMutationSession, GraphEnsureCapacitySegmentWriteSummary,
    GraphEnsureCapacityWriteSummary, GraphInsertDecision, GraphInsertPolicy, GraphInsertResult,
    GraphInsertSegmentWriteSummary, GraphInsertWriteSummary, GraphLocalRebalanceDelta,
    GraphLocalRebalancePlan, GraphMaintenanceBatchWriteSummary, GraphMaintenanceCandidate,
    GraphMaintenanceCyclePlan, GraphMaintenanceCycleWriteSummary, GraphMaintenanceWorkItem,
    GraphMutationPath, GraphRebalancePlan, GraphRuntime, HydratedSurfaceRuntimes, HydrationError,
    LogicalEdgeLocator, RebalanceInsertSpec, RebalancePrepareSpec, RegionKind, RegionManager,
    ResolvedEdgeSlot, ReverseSurfaceRuntime, ShardCanisterDirectory, ShardDirectoryStore,
    SurfaceVertexWindowReserveHint, SurfaceVertexWindowSummary, VertexEntry, VertexRef, WasmPages,
    WritebackError, estimate_vertex_window_reserve_hint_from_stable_memory,
    forward_surface_from_layout, hydrate_surface_runtimes_from_stable_memory,
    read_edge_entries_by_ref_from_stable_memory, read_vertex_base_edge_ref_from_stable_memory,
    read_vertex_base_entries_from_stable_memory, read_vertex_base_entry_from_stable_memory,
    read_vertex_entries_from_stable_memory, read_vertex_entry_by_ref_from_stable_memory,
    read_vertex_entry_from_stable_memory, read_vertex_reserved_base_entries_from_stable_memory,
    read_vertex_reserved_span_len_from_stable_memory, reverse_surface_from_layout,
    summarize_vertex_window_from_stable_memory, write_surface_runtimes_to_stable_memory,
};
use crate::observability::{
    format_last_write_event, format_maintenance_queue, format_maintenance_queue_storage,
    format_write_event_history,
};
use crate::property_index::{
    FixedSlotPropertyEqualityMap, PropertyIndex, PropertyIndexEntityKind, PropertyIndexEntry,
    PropertyIndexError, PropertyIndexKey, PropertyIndexNodeStoreMutationKind,
    open_fixed_slot_property_equality_map, snapshot_fixed_slot_property_equality_map,
    snapshot_from_equality_any_memory, snapshot_from_equality_map,
};
use crate::property_store::{
    FixedSlotGraphPropertyStableMap, PropertyKey, PropertyStoreError,
    StoredPropertyValue, btree_distinct_property_names, btree_get_edge_property,
    btree_get_node_property, btree_scan_entities, btree_scan_entities_property_subset,
    btree_scan_entity, default_property_region_chain,
    open_fixed_slot_edge_property_store, open_fixed_slot_node_property_store,
    snapshot_fixed_slot_graph_property_store,
};
pub use errors::{GraphStoreError, GraphStoreResult};
pub use facade_types::{
    GraphStoreAppendVertexWriteSummary, GraphStoreAppendVerticesWriteSummary,
    GraphStoreBootstrapEdgeProjection, GraphStoreBootstrapEdgeWriteSummary,
    GraphStoreBootstrapGraphProjection, GraphStoreBootstrapGraphWriteSummary,
    GraphStoreBootstrapVerticesProjection, GraphStoreEdgeLogicalLocatorMapping,
    GraphStoreEdgeWriteOperation, GraphStoreEdgeWriteProjection,
    GraphStoreEnsureCapacityProjection, GraphStoreFacadeWriteEvent, GraphStoreInsertEdgeProjection,
    GraphStoreMaintenanceBatchProjection, GraphStoreMaintenanceCycleProjection,
    GraphStoreMaintenanceQueueAction, GraphStoreMaintenanceQueueItemProjection,
    GraphStoreMaintenanceQueueProjection, GraphStoreMaintenanceQueueStorageProjection,
    GraphStoreMutationWriteSummary, GraphStoreNodeDeleteProjection, GraphStoreProductionMetrics,
    GraphStoreProductionMetricsSnapshot, GraphStorePropertyIndexMutationSummary,
    GraphStorePropertyIndexTouchedSections, GraphStorePropertyMutationWriteSummary,
    GraphStorePropertyWriteProjection, GraphStoreRefreshedVertices, GraphStoreVertexOrdinalMapping,
    GraphStoreWriteEventProjection, PropertyIndexFallbackReason,
};

type GraphStoreReplaceEdgeSummary =
    GraphStoreMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>;

/// Root memory for fixed-slot property and PIDX maps (`MemoryManager` over the reserved page range).
pub type GraphStorePropertySlotRoot<M> = PageRangeMemory<RcGraphMemory<M>>;

/// Node property btree in fixed slot 8 (same encoding as legacy PSB1 region; header unused on this path).
pub type GraphStoreNodePropertyMap<M> = FixedSlotGraphPropertyStableMap<GraphStorePropertySlotRoot<M>>;

/// Edge property btree in fixed slot 9.
pub type GraphStoreEdgePropertyMap<M> = FixedSlotGraphPropertyStableMap<GraphStorePropertySlotRoot<M>>;

/// PIDX equality btree in fixed slot 10.
pub type GraphStorePropertyEqualityFixedMap<M> =
    FixedSlotPropertyEqualityMap<GraphStorePropertySlotRoot<M>>;

#[cfg(test)]
thread_local! {
    /// When true, the next node-side property index mutation path on **this thread** returns
    /// [`PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage`] (test-only).
    pub(crate) static FAIL_NEXT_NODE_PROPERTY_INDEX_SYNC_TEST: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    /// When true, the next edge-side property index mutation path on **this thread** returns
    /// [`PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage`] (test-only).
    pub(crate) static FAIL_NEXT_EDGE_PROPERTY_INDEX_SYNC_TEST: Cell<bool> = const { Cell::new(false) };
}

/// Primary typed entrypoint for graph persistence in `graph-store`.
///
/// This facade owns the region-manager metadata together with the hydrated
/// graph runtime, while keeping stable-memory access explicit at method call
/// sites. The goal is to keep the low-level-first model visible while avoiding
/// repetitive wiring for callers.
pub struct GraphStore<M: Memory + Clone = VecMemory> {
    /// Region metadata and allocator-side state.
    pub manager: Rc<RefCell<RegionManager>>,
    /// Canonical stable-memory backing (shared with PIDX btree subregion I/O).
    pub memory: Rc<M>,
    /// Fixed-slot [`MemoryManager`] view for node/edge property stores and PIDX (slots 8–10).
    pub property_slots: GraphStoreMemorySlots<GraphStorePropertySlotRoot<M>>,
    /// In-memory forward/reverse adjacency runtime plus locator sidecar.
    pub graph: GraphRuntime,
    /// Node properties in fixed stable slot 8 ([`StableBTreeMap`]).
    pub node_property_store: GraphStoreNodePropertyMap<M>,
    /// Edge properties in fixed stable slot 9.
    pub edge_property_store: GraphStoreEdgePropertyMap<M>,
    /// Derived equality index for node properties.
    pub node_property_index: PropertyIndex,
    /// Derived equality index for edge properties.
    pub edge_property_index: PropertyIndex,
    /// Persisted equality btree in fixed stable slot 10 (PIDX keys; btree bytes only).
    pub property_equality_map: GraphStorePropertyEqualityFixedMap<M>,
    pub property_index_dirty: bool,
    /// Whether the node property region header may be out of sync with the btree length cell.
    pub node_property_store_dirty: bool,
    /// Whether the edge property region header may be out of sync with the btree length cell.
    pub edge_property_store_dirty: bool,
    /// Most recent facade-level write event.
    pub last_write_event: Option<GraphStoreFacadeWriteEvent>,
    /// Recent facade-level write events in observation order.
    pub write_history: Vec<GraphStoreFacadeWriteEvent>,
    /// In-process production-facing metrics for property/index paths.
    pub production_metrics: GraphStoreProductionMetrics,
    /// Cross-canister principal table for [`EdgeMeta::is_shard_canister`](crate::low_level::edge::EdgeMeta::is_shard_canister) payloads.
    pub shard_canister_directory: ShardCanisterDirectory,
}

impl<M: Memory + Clone> std::fmt::Debug for GraphStore<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphStore")
            .field("manager", &self.manager)
            .field("memory", &"...")
            .field("graph", &self.graph)
            .field("node_property_store_len", &self.node_property_store.len())
            .field("edge_property_store_len", &self.edge_property_store.len())
            .field("node_property_index", &self.node_property_index)
            .field("edge_property_index", &self.edge_property_index)
            .field("property_equality_map_len", &self.property_equality_map.len())
            .field("property_index_dirty", &self.property_index_dirty)
            .field("node_property_store_dirty", &self.node_property_store_dirty)
            .field("edge_property_store_dirty", &self.edge_property_store_dirty)
            .field("last_write_event", &self.last_write_event)
            .field("write_history_len", &self.write_history.len())
            .field("production_metrics", &self.production_metrics)
            .field(
                "shard_canister_directory_len",
                &self.shard_canister_directory.len(),
            )
            .finish()
    }
}

impl<M: Memory + Clone> Clone for GraphStore<M> {
    fn clone(&self) -> Self {
        let manager = Rc::new(RefCell::new(self.manager.borrow().clone()));
        let memory = Rc::new((*self.memory).clone());
        let property_slots =
            GraphStoreMemorySlots::for_root_memory(RcGraphMemory(Rc::clone(&memory)));
        let mut node_property_store = open_fixed_slot_node_property_store(&property_slots);
        let mut edge_property_store = open_fixed_slot_edge_property_store(&property_slots);
        let mut property_equality_map = open_fixed_slot_property_equality_map(&property_slots);
        for e in self.property_equality_map.iter() {
            property_equality_map.insert(e.key().clone(), e.value().clone());
        }
        for e in self.node_property_store.iter() {
            node_property_store.insert(e.key().clone(), e.value().clone());
        }
        for e in self.edge_property_store.iter() {
            edge_property_store.insert(e.key().clone(), e.value().clone());
        }
        Self {
            manager,
            memory,
            property_slots,
            graph: self.graph.clone(),
            node_property_store,
            edge_property_store,
            node_property_index: self.node_property_index.clone(),
            edge_property_index: self.edge_property_index.clone(),
            property_equality_map,
            property_index_dirty: self.property_index_dirty,
            node_property_store_dirty: self.node_property_store_dirty,
            edge_property_store_dirty: self.edge_property_store_dirty,
            last_write_event: self.last_write_event.clone(),
            write_history: self.write_history.clone(),
            production_metrics: self.production_metrics.clone(),
            shard_canister_directory: self.shard_canister_directory.clone(),
        }
    }
}

struct AssembledAfterPropertyLoadArgs<M: Memory + Clone> {
    manager: Rc<RefCell<RegionManager>>,
    memory: Rc<M>,
    property_slots: GraphStoreMemorySlots<GraphStorePropertySlotRoot<M>>,
    graph: GraphRuntime,
    node_property_store: GraphStoreNodePropertyMap<M>,
    edge_property_store: GraphStoreEdgePropertyMap<M>,
    node_property_index: PropertyIndex,
    edge_property_index: PropertyIndex,
    property_equality_map: GraphStorePropertyEqualityFixedMap<M>,
    property_index_dirty: bool,
    shard_canister_directory: ShardCanisterDirectory,
}

/// Thin facade-level batch mutation session.
///
/// This wraps the low-level `GraphBatchMutationSession` so callers that start
/// from `GraphStore` do not need to wire the manager and graph runtime
/// manually for each batch.
pub struct GraphStoreBatchSession<'a, M: Memory> {
    inner: GraphBatchMutationSession<'a, M>,
}

/// Thin higher-level adapter that binds one [`GraphStoreStore`] together with
/// one stable-memory handle.
///
/// This keeps upper layers from threading `memory` through every facade call
/// while still reusing the facade trait boundary instead of depending
/// directly on [`GraphStore`].
pub struct GraphStoreStoreAdapter<'a, S: GraphStoreStore> {
    store: &'a mut S,
    memory: &'a S::Mem,
}

/// Higher-level service boundary over one bound graph store.
///
/// Unlike [`GraphStoreStore`], this trait assumes stable memory is already
/// bound, so upper layers can express bootstrap and mutation flows without
/// threading a `Memory` handle through every call.
pub trait GraphStoreService {
    /// Returns the most recent facade-level write event observed through this service.
    fn last_write_event(&self) -> Option<&GraphStoreFacadeWriteEvent>;

    /// Returns recent facade-level write events in observation order.
    fn write_history(&self) -> &[GraphStoreFacadeWriteEvent];

    /// Returns recent facade-level write events projected into shared diagnostics history.
    fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(
            &self
                .write_history()
                .iter()
                .flat_map(GraphStoreFacadeWriteEvent::shared_projections)
                .collect::<Vec<_>>(),
        )
    }

    /// Returns the most recent facade-level write event projected into one diagnostics line.
    fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(
            &self
                .write_history()
                .iter()
                .flat_map(GraphStoreFacadeWriteEvent::shared_projections)
                .collect::<Vec<_>>(),
        )
    }

    /// Bootstraps multiple vertex refs and initial edges.
    fn bootstrap_vertex_refs_and_edges(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> GraphStoreResult<GraphStoreBootstrapGraphWriteSummary>;

    /// Inserts one logical edge.
    fn insert_edge_pair_with_local_rebalance(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
    ) -> Result<GraphInsertWriteSummary, WritebackError>;

    /// Replaces one logical edge.
    fn replace_edge_pair(
        &mut self,
        spec: EdgeReplaceSpec,
    ) -> Result<GraphStoreReplaceEdgeSummary, WritebackError>;

    /// Tombstones one logical edge.
    fn tombstone_edge_pair(
        &mut self,
        spec: EdgeTombstoneSpec,
    ) -> Result<GraphStoreMutationWriteSummary<GraphMutationPath>, WritebackError>;

    /// Flushes dirty state.
    fn flush_dirty(&mut self) -> GraphStoreResult<GraphStoreRefreshedVertices>;
}

/// Store trait for the graph-store facade.
///
/// This is intentionally small and non-object-safe. The goal is simply to let
/// upper layers depend on a stable facade-shaped contract while the concrete
/// storage layout keeps evolving.
pub trait GraphStoreStore {
    /// Stable-memory type backing this store (same as the bound handle passed to write/hydrate paths).
    type Mem: Memory + Clone;

    /// Returns the most recent facade-level write event observed through this store.
    fn last_write_event(&self) -> Option<&GraphStoreFacadeWriteEvent>;

    /// Returns recent facade-level write events in observation order.
    fn write_history(&self) -> &[GraphStoreFacadeWriteEvent];

    /// Returns recent facade-level write events projected into shared diagnostics history.
    fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(
            &self
                .write_history()
                .iter()
                .flat_map(GraphStoreFacadeWriteEvent::shared_projections)
                .collect::<Vec<_>>(),
        )
    }

    /// Returns the most recent facade-level write event projected into one diagnostics line.
    fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(
            &self
                .write_history()
                .iter()
                .flat_map(GraphStoreFacadeWriteEvent::shared_projections)
                .collect::<Vec<_>>(),
        )
    }

    /// Returns immutable region-manager metadata.
    fn manager(&self) -> Ref<'_, RegionManager>;

    /// Returns mutable region-manager metadata.
    fn manager_mut(&mut self) -> RefMut<'_, RegionManager>;

    /// Returns the underlying graph runtime.
    fn graph(&self) -> &GraphRuntime;

    /// Returns mutable access to the underlying graph runtime.
    fn graph_mut(&mut self) -> &mut GraphRuntime;

    fn resolve_forward_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot>;

    fn resolve_reverse_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot>;

    fn choose_insert_decision_with_incoming_live_entries(
        &self,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        incoming_live_entries: usize,
    ) -> Option<GraphInsertDecision>;

    fn plan_local_rebalance(&self, plan: GraphRebalancePlan) -> Option<GraphLocalRebalancePlan>;

    fn build_local_rebalance_delta(
        &self,
        plan: GraphLocalRebalancePlan,
    ) -> Option<GraphLocalRebalanceDelta>;

    /// Cross-canister principal directory persisted with the graph facade.
    fn shard_canister_directory(&self) -> &ShardCanisterDirectory;

    /// Mutable shard directory (kept in sync on flush/write-all paths).
    fn shard_canister_directory_mut(&mut self) -> &mut ShardCanisterDirectory;

    /// Returns immutable access to the stable node property map.
    fn node_property_store(&self) -> &GraphStoreNodePropertyMap<Self::Mem>;

    /// Returns mutable access to the stable node property map.
    fn node_property_store_mut(&mut self) -> &mut GraphStoreNodePropertyMap<Self::Mem>;

    /// Returns immutable access to the stable edge property map.
    fn edge_property_store(&self) -> &GraphStoreEdgePropertyMap<Self::Mem>;

    /// Returns mutable access to the stable edge property map.
    fn edge_property_store_mut(&mut self) -> &mut GraphStoreEdgePropertyMap<Self::Mem>;

    /// Returns the latest node properties for one semantic node id.
    fn scan_node_properties(&self, node_id: NodeId) -> PropertyMap;

    /// Returns the latest edge properties for one semantic edge id.
    fn scan_edge_properties(&self, edge_id: EdgeId) -> PropertyMap;

    /// Latest node properties for many ids in one forward scan of the node property log.
    fn scan_node_properties_batch(&self, node_ids: &[NodeId]) -> BTreeMap<NodeId, PropertyMap>;

    /// Like [`Self::scan_node_properties_batch`], but only materializes the listed property names.
    ///
    /// An empty `property_names` set yields empty [`PropertyMap`] values without scanning keys.
    fn scan_node_properties_batch_subset(
        &self,
        node_ids: &[NodeId],
        property_names: &BTreeSet<String>,
    ) -> BTreeMap<NodeId, PropertyMap>;

    /// Like [`Self::scan_edge_properties`] batched, restricted to `property_names`.
    fn scan_edge_properties_batch_subset(
        &self,
        edge_ids: &[EdgeId],
        property_names: &BTreeSet<String>,
    ) -> BTreeMap<EdgeId, PropertyMap>;

    fn get_node_property_value(&self, node_id: NodeId, property: &str) -> Option<Value>;

    fn get_edge_property_value(&self, edge_id: EdgeId, property: &str) -> Option<Value>;

    fn distinct_node_property_names(&self) -> BTreeSet<String>;

    fn distinct_edge_property_names(&self) -> BTreeSet<String>;

    /// Returns node ids matching one exact equality property predicate.
    fn scan_node_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<NodeId>;

    /// Returns node ids that have any binding for the given property name.
    fn scan_node_ids_by_property(&self, property: &str) -> Vec<NodeId>;

    /// Returns edge ids that have any binding for the given property name.
    fn scan_edge_ids_by_property(&self, property: &str) -> Vec<EdgeId>;

    /// Returns edge ids matching one exact equality property predicate.
    fn scan_edge_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<EdgeId>;

    /// Returns whether the node-side property state has unflushed changes.
    fn node_property_store_is_dirty(&self) -> bool;

    /// Returns whether the edge-side property state has unflushed changes.
    fn edge_property_store_is_dirty(&self) -> bool;

    /// Appends or overwrites one node property in the stable property store.
    fn set_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError>;

    /// Appends one node-property tombstone in the stable property store.
    fn remove_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<(), PropertyStoreError>;

    /// Appends or overwrites one edge property in the stable property store.
    fn set_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError>;

    /// Appends one edge-property tombstone in the stable property store.
    fn remove_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<(), PropertyStoreError>;

    /// Like [`Self::set_node_property_value`], plus a structured property-index mutation summary.
    fn set_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError>;

    /// Like [`Self::remove_node_property_value`], plus a structured property-index mutation summary.
    fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError>;

    /// Like [`Self::set_edge_property_value`], plus a structured property-index mutation summary.
    fn set_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError>;

    /// Like [`Self::remove_edge_property_value`], plus a structured property-index mutation summary.
    fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError>;

    /// Appends or overwrites one node property, then flushes dirty state.
    fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary>;

    /// Appends one node-property tombstone, then flushes dirty state.
    fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary>;

    /// Appends or overwrites one edge property, then flushes dirty state.
    fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary>;

    /// Appends one edge-property tombstone, then flushes dirty state.
    fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary>;

    /// Rebuilds the canonical logical-locator sidecar from externally supplied forward-side ids.
    fn try_rebuild_logical_locator_sidecar(
        &mut self,
        forward_vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> GraphStoreResult<()>;

    /// Writes the full graph runtime state back to stable memory.
    fn try_write_all_to_stable_memory(&mut self, memory: &impl Memory) -> GraphStoreResult<()>;

    /// Refreshes dirty state and writes it back.
    fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> GraphStoreResult<(Vec<usize>, Vec<usize>)>;

    /// Appends one empty vertex slot pair to both surfaces.
    fn append_empty_vertex_pair(&mut self) -> GraphStoreResult<(usize, usize)>;

    /// Appends `count` empty vertex slot pairs to both surfaces.
    fn append_empty_vertex_pairs(&mut self, count: usize) -> GraphStoreResult<Vec<(usize, usize)>>;

    /// Bootstraps multiple new vertex slots plus initial logical edges.
    ///
    /// This is the canonical facade/bootstrap entrypoint. High-level callers
    /// that still speak semantic `NodeId` should convert at the integration
    /// boundary before crossing into the facade.
    fn bootstrap_vertex_refs_and_edges_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStoreBootstrapGraphWriteSummary>;

    /// Inserts one logical edge and performs one local rebalance cycle first if needed.
    fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError>;

    /// Replaces one logical edge and writes back dirty state.
    fn replace_edge_pair_and_write(
        &mut self,
        spec: EdgeReplaceSpec,
        memory: &impl Memory,
    ) -> Result<GraphStoreReplaceEdgeSummary, WritebackError>;

    /// Tombstones one logical edge and writes back dirty state.
    fn tombstone_edge_pair_and_write(
        &mut self,
        spec: EdgeTombstoneSpec,
        memory: &impl Memory,
    ) -> Result<GraphStoreMutationWriteSummary<GraphMutationPath>, WritebackError>;
}

const FACADE_WRITE_HISTORY_LIMIT: usize = 16;

impl<M: Memory + Clone> GraphStore<M> {
    fn fixed_memory_slots(&self) -> GraphStoreMemorySlots<PageRangeMemory<BorrowedMemory<'_, M>>> {
        GraphStoreMemorySlots::for_root_memory(BorrowedMemory::new(self.memory.as_ref()))
    }

    fn open_fixed_slot_node_property_store(
        &self,
    ) -> FixedSlotGraphPropertyStableMap<PageRangeMemory<BorrowedMemory<'_, M>>> {
        open_fixed_slot_node_property_store(&self.fixed_memory_slots())
    }

    fn open_fixed_slot_edge_property_store(
        &self,
    ) -> FixedSlotGraphPropertyStableMap<PageRangeMemory<BorrowedMemory<'_, M>>> {
        open_fixed_slot_edge_property_store(&self.fixed_memory_slots())
    }

    fn open_fixed_slot_property_equality_map(
        &self,
    ) -> FixedSlotPropertyEqualityMap<PageRangeMemory<BorrowedMemory<'_, M>>> {
        open_fixed_slot_property_equality_map(&self.fixed_memory_slots())
    }

    fn sync_property_indices_from_equality_btree(&mut self) {
        let snap = snapshot_from_equality_any_memory(&self.property_equality_map, 64);
        self.node_property_index = snap.node_index;
        self.edge_property_index = snap.edge_index;
        self.node_property_store_dirty = false;
        self.edge_property_store_dirty = false;
        self.property_index_dirty = false;
    }

    fn sync_shadow_property_state_from_fixed_slots(&mut self) {
        self.sync_property_indices_from_equality_btree();
    }

    fn record_write_event(&mut self, event: GraphStoreFacadeWriteEvent) {
        self.last_write_event = Some(event.clone());
        self.write_history.push(event);
        if self.write_history.len() > FACADE_WRITE_HISTORY_LIMIT {
            self.write_history.remove(0);
        }
    }

    /// Returns the most recent facade-level write event.
    pub fn last_write_event(&self) -> Option<&GraphStoreFacadeWriteEvent> {
        self.last_write_event.as_ref()
    }

    /// Returns recent facade-level write events in observation order.
    pub fn write_history(&self) -> &[GraphStoreFacadeWriteEvent] {
        &self.write_history
    }

    /// Returns the recent façade write history projected onto the shared event vocabulary.
    pub fn shared_write_history(&self) -> Vec<GraphStoreWriteEventProjection> {
        self.write_history
            .iter()
            .flat_map(GraphStoreFacadeWriteEvent::shared_projections)
            .collect()
    }

    /// Returns the recent façade write history formatted as compact diagnostics lines.
    pub fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(&self.shared_write_history())
    }

    pub fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(&self.shared_write_history())
    }

    /// Returns the retained maintenance queue as structured projections.
    pub fn maintenance_queue_projection(&self) -> Vec<GraphStoreMaintenanceQueueItemProjection> {
        self.graph
            .maintenance_queue()
            .iter()
            .copied()
            .map(GraphStoreMaintenanceQueueItemProjection::from_work_item)
            .collect()
    }

    /// Returns the retained maintenance queue formatted as diagnostics lines.
    pub fn formatted_maintenance_queue(&self) -> Vec<String> {
        format_maintenance_queue(&self.maintenance_queue_projection())
    }

    /// Reads the persisted maintenance queue directly from stable memory.
    pub fn try_read_maintenance_queue_from_stable_memory(
        &self,
        _memory: &impl Memory,
    ) -> GraphStoreResult<Vec<GraphMaintenanceWorkItem>> {
        Self::load_maintenance_queue_from_slot(&self.property_slots)
    }

    /// Reads the persisted maintenance queue directly from stable memory as structured projections.
    pub fn try_read_maintenance_queue_projection_from_stable_memory(
        &self,
        memory: &impl Memory,
    ) -> GraphStoreResult<Vec<GraphStoreMaintenanceQueueItemProjection>> {
        Ok(self
            .try_read_maintenance_queue_from_stable_memory(memory)?
            .into_iter()
            .map(GraphStoreMaintenanceQueueItemProjection::from_work_item)
            .collect())
    }

    /// Reads the persisted maintenance queue directly from stable memory as formatted diagnostics lines.
    pub fn try_format_maintenance_queue_from_stable_memory(
        &self,
        memory: &impl Memory,
    ) -> GraphStoreResult<Vec<String>> {
        Ok(format_maintenance_queue(
            &self.try_read_maintenance_queue_projection_from_stable_memory(memory)?,
        ))
    }

    /// Reads metadata for the persisted maintenance queue directly from stable memory.
    pub fn try_read_maintenance_queue_storage_projection_from_stable_memory(
        &self,
        _memory: &impl Memory,
    ) -> GraphStoreResult<GraphStoreMaintenanceQueueStorageProjection> {
        let bytes = Self::read_maintenance_queue_slot_bytes(&self.property_slots)?;
        let logical_len_bytes = u64::try_from(bytes.len()).map_err(|_| {
            GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                RegionKind::MaintenanceQueue,
                u64::MAX,
            ))
        })?;
        if bytes.is_empty() {
            return Ok(GraphStoreMaintenanceQueueStorageProjection {
                logical_len_bytes: 0,
                queue_len: 0,
                format_version: None,
                stored_checksum: None,
                computed_checksum: None,
                checksum_valid: None,
            });
        }
        if bytes.len() < Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN {
            return Err(GraphStoreError::Hydration(
                HydrationError::InvalidMaintenanceQueueHeader(RegionKind::MaintenanceQueue),
            ));
        }
        if bytes[..4] != Self::SERIALIZED_MAINTENANCE_QUEUE_MAGIC {
            return Err(GraphStoreError::Hydration(
                HydrationError::InvalidMaintenanceQueueHeader(RegionKind::MaintenanceQueue),
            ));
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().expect("queue version"));
        if version != Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION {
            return Err(GraphStoreError::Hydration(
                HydrationError::UnsupportedFormatVersion {
                    kind: RegionKind::MaintenanceQueue,
                    expected: Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION,
                    actual: version,
                },
            ));
        }
        let queue_len = u64::from_le_bytes(bytes[8..16].try_into().expect("queue count")) as usize;
        let stored_checksum = u64::from_le_bytes(bytes[16..24].try_into().expect("queue checksum"));
        let body = &bytes[Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN..];
        let computed_checksum = Self::maintenance_queue_checksum(body);
        Ok(GraphStoreMaintenanceQueueStorageProjection {
            logical_len_bytes,
            queue_len,
            format_version: Some(version),
            stored_checksum: Some(stored_checksum),
            computed_checksum: Some(computed_checksum),
            checksum_valid: Some(stored_checksum == computed_checksum),
        })
    }

    /// Reads metadata for the persisted maintenance queue directly from stable memory as one diagnostics line.
    pub fn try_format_maintenance_queue_storage_from_stable_memory(
        &self,
        memory: &impl Memory,
    ) -> GraphStoreResult<String> {
        Ok(format_maintenance_queue_storage(
            &self.try_read_maintenance_queue_storage_projection_from_stable_memory(memory)?,
        ))
    }

    /// Returns the retained in-memory maintenance queue storage view as one diagnostics line.
    pub fn formatted_maintenance_queue_storage(&self) -> String {
        let queue = self.graph.maintenance_queue();
        let stored_checksum = Self::maintenance_queue_checksum(
            &Self::encode_maintenance_queue(queue)
                .expect("maintenance queue encoding should succeed")
                [Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN..],
        );
        format_maintenance_queue_storage(&GraphStoreMaintenanceQueueStorageProjection {
            logical_len_bytes: Self::maintenance_queue_serialized_len(queue.len())
                .expect("maintenance queue serialized len should fit"),
            queue_len: queue.len(),
            format_version: Some(Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION),
            stored_checksum: Some(stored_checksum),
            computed_checksum: Some(stored_checksum),
            checksum_valid: Some(true),
        })
    }
}

impl<M: Memory + Clone> GraphStore<M> {
    const SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN: usize = 24;
    const SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN: usize = 56;
    const MAINTENANCE_QUEUE_LAST_EPOCH_NONE: u64 = u64::MAX;
    const SERIALIZED_MAINTENANCE_QUEUE_MAGIC: [u8; 4] = *b"MGQ1";
    const SERIALIZED_MAINTENANCE_QUEUE_VERSION: u32 = 1;

    fn maintenance_queue_serialized_len(queue_len: usize) -> GraphStoreResult<u64> {
        let item_bytes = queue_len
            .checked_mul(Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN)
            .and_then(|n| n.checked_add(Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN))
            .ok_or({
                GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    queue_len as u64,
                ))
            })?;
        u64::try_from(item_bytes).map_err(|_| {
            GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                RegionKind::MaintenanceQueue,
                queue_len as u64,
            ))
        })
    }

    fn encode_maintenance_queue(queue: &[GraphMaintenanceWorkItem]) -> GraphStoreResult<Vec<u8>> {
        let count = u64::try_from(queue.len()).map_err(|_| {
            GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                RegionKind::MaintenanceQueue,
                queue.len() as u64,
            ))
        })?;
        let mut body =
            Vec::with_capacity(queue.len() * Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN);
        for item in queue {
            body.extend_from_slice(&u64::from(item.vertex_ref).to_le_bytes());
            body.extend_from_slice(&(item.anchor_ordinal as u64).to_le_bytes());
            body.extend_from_slice(&(item.start_ordinal as u64).to_le_bytes());
            body.extend_from_slice(&(item.end_ordinal_exclusive as u64).to_le_bytes());
            body.extend_from_slice(&item.priority_score.to_le_bytes());
            body.extend_from_slice(
                &item
                    .last_maintenance_epoch
                    .unwrap_or(Self::MAINTENANCE_QUEUE_LAST_EPOCH_NONE)
                    .to_le_bytes(),
            );
            body.extend_from_slice(&item.recent_maintenance_penalty.to_le_bytes());
        }
        let checksum = Self::maintenance_queue_checksum(&body);
        let mut bytes =
            Vec::with_capacity(Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN + body.len());
        bytes.extend_from_slice(&Self::SERIALIZED_MAINTENANCE_QUEUE_MAGIC);
        bytes.extend_from_slice(&Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes.extend_from_slice(&body);
        Ok(bytes)
    }

    fn maintenance_queue_checksum(bytes: &[u8]) -> u64 {
        const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x00000100000001B3;

        let mut hash = FNV_OFFSET_BASIS;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    fn decode_maintenance_queue(bytes: &[u8]) -> GraphStoreResult<Vec<GraphMaintenanceWorkItem>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        if bytes.len() < Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN {
            return Err(GraphStoreError::Hydration(
                HydrationError::InvalidMaintenanceQueueHeader(RegionKind::MaintenanceQueue),
            ));
        }
        if bytes[..4] != Self::SERIALIZED_MAINTENANCE_QUEUE_MAGIC {
            return Err(GraphStoreError::Hydration(
                HydrationError::InvalidMaintenanceQueueHeader(RegionKind::MaintenanceQueue),
            ));
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().expect("queue version"));
        if version != Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION {
            return Err(GraphStoreError::Hydration(
                HydrationError::UnsupportedFormatVersion {
                    kind: RegionKind::MaintenanceQueue,
                    expected: Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION,
                    actual: version,
                },
            ));
        }
        let count = u64::from_le_bytes(bytes[8..16].try_into().expect("queue item count")) as usize;
        let checksum = u64::from_le_bytes(bytes[16..24].try_into().expect("queue checksum"));
        let body = &bytes[Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN..];
        let expected = count
            .checked_mul(Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN)
            .ok_or({
                GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    body.len() as u64,
                ))
            })?;
        if body.len() != expected {
            return Err(GraphStoreError::Hydration(HydrationError::InvalidLength {
                kind: RegionKind::MaintenanceQueue,
                expected_multiple: Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN,
                actual: body.len(),
            }));
        }
        let actual_checksum = Self::maintenance_queue_checksum(body);
        if checksum != actual_checksum {
            return Err(GraphStoreError::Hydration(
                HydrationError::ChecksumMismatch {
                    kind: RegionKind::MaintenanceQueue,
                    expected: checksum,
                    actual: actual_checksum,
                },
            ));
        }
        let mut queue = Vec::with_capacity(count);
        for chunk in body.chunks_exact(Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN) {
            let vertex =
                NodeId::try_from(u64::from_le_bytes(chunk[0..8].try_into().expect("vertex")))
                    .map_err(|_| {
                        GraphStoreError::Hydration(HydrationError::InvalidLength {
                            kind: RegionKind::MaintenanceQueue,
                            expected_multiple: Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN,
                            actual: body.len(),
                        })
                    })?;
            let anchor_ordinal =
                usize::try_from(u64::from_le_bytes(chunk[8..16].try_into().expect("anchor")))
                    .map_err(|_| {
                        GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                            RegionKind::MaintenanceQueue,
                            u64::MAX,
                        ))
                    })?;
            let window_start_ordinal = usize::try_from(u64::from_le_bytes(
                chunk[16..24].try_into().expect("window start"),
            ))
            .map_err(|_| {
                GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    u64::MAX,
                ))
            })?;
            let window_end_ordinal_exclusive = usize::try_from(u64::from_le_bytes(
                chunk[24..32].try_into().expect("window end"),
            ))
            .map_err(|_| {
                GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    u64::MAX,
                ))
            })?;
            let priority_score = u64::from_le_bytes(chunk[32..40].try_into().expect("priority"));
            let last_epoch_raw = u64::from_le_bytes(chunk[40..48].try_into().expect("last epoch"));
            let recent_maintenance_penalty =
                u64::from_le_bytes(chunk[48..56].try_into().expect("recent penalty"));
            queue.push(GraphMaintenanceWorkItem {
                vertex_ref: vertex.into(),
                anchor_ordinal,
                start_ordinal: window_start_ordinal,
                end_ordinal_exclusive: window_end_ordinal_exclusive,
                priority_score,
                last_maintenance_epoch: if last_epoch_raw == Self::MAINTENANCE_QUEUE_LAST_EPOCH_NONE
                {
                    None
                } else {
                    Some(last_epoch_raw)
                },
                recent_maintenance_penalty,
            });
        }
        Ok(queue)
    }

    pub(crate) fn read_maintenance_queue_slot_bytes(
        slots: &GraphStoreMemorySlots<GraphStorePropertySlotRoot<M>>,
    ) -> GraphStoreResult<Vec<u8>> {
        let mem = slots.maintenance_queue();
        let pages = mem.size();
        if pages == 0 {
            return Ok(Vec::new());
        }
        let total = pages
            .checked_mul(crate::low_level::WASM_PAGE_SIZE)
            .ok_or_else(|| {
                GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    u64::MAX,
                ))
            })?;
        let len = usize::try_from(total).map_err(|_| {
            GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                RegionKind::MaintenanceQueue,
                total,
            ))
        })?;
        let mut bytes = vec![0u8; len];
        mem.read(0, &mut bytes);
        if bytes.len() >= Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN
            && bytes[..4] == Self::SERIALIZED_MAINTENANCE_QUEUE_MAGIC
        {
            let count = u64::from_le_bytes(bytes[8..16].try_into().expect("queue count")) as usize;
            let body_len = count
                .checked_mul(Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN)
                .ok_or_else(|| {
                    GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                        RegionKind::MaintenanceQueue,
                        count as u64,
                    ))
                })?;
            let need = Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN
                .checked_add(body_len)
                .ok_or_else(|| {
                    GraphStoreError::Hydration(HydrationError::RegionTooLarge(
                        RegionKind::MaintenanceQueue,
                        u64::MAX,
                    ))
                })?;
            if need <= bytes.len() {
                bytes.truncate(need);
            }
        }
        Ok(bytes)
    }

    pub(crate) fn write_maintenance_queue_slot_bytes(
        slots: &GraphStoreMemorySlots<GraphStorePropertySlotRoot<M>>,
        bytes: &[u8],
    ) -> Result<(), WritebackError> {
        let mem = slots.maintenance_queue();
        let needed_pages = (bytes.len() as u64).div_ceil(crate::low_level::WASM_PAGE_SIZE);
        while mem.size() < needed_pages {
            if mem.grow(1) < 0 {
                return Err(WritebackError::MemoryGrowFailed {
                    current_pages: mem.size(),
                    delta_pages: 1,
                });
            }
        }
        if !bytes.is_empty() {
            mem.write(0, bytes);
        }
        Ok(())
    }

    fn write_maintenance_queue_to_stable_memory(
        &mut self,
        _memory: &impl Memory,
    ) -> Result<u64, WritebackError> {
        let bytes =
            Self::encode_maintenance_queue(self.graph.maintenance_queue()).map_err(|_| {
                WritebackError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    self.graph.maintenance_queue().len() as u64,
                )
            })?;
        Self::write_maintenance_queue_slot_bytes(&self.property_slots, &bytes)?;
        Ok(bytes.len() as u64)
    }

    fn load_maintenance_queue_from_slot(
        slots: &GraphStoreMemorySlots<GraphStorePropertySlotRoot<M>>,
    ) -> GraphStoreResult<Vec<GraphMaintenanceWorkItem>> {
        let bytes = Self::read_maintenance_queue_slot_bytes(slots)?;
        Self::decode_maintenance_queue(&bytes)
    }

    fn write_shard_canister_directory_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> Result<u64, WritebackError> {
        let slots = GraphStoreMemorySlots::for_root_memory(BorrowedMemory::new(memory));
        let mut store = ShardDirectoryStore::open(&slots);
        store.replace_from_directory(&self.shard_canister_directory);
        Ok(self.shard_canister_directory.len() as u64)
    }

    fn load_shard_canister_directory_from_stable_memory(
        manager: &RegionManager,
        memory: &impl Memory,
    ) -> GraphStoreResult<ShardCanisterDirectory> {
        let _ = manager;
        let slots = GraphStoreMemorySlots::for_root_memory(BorrowedMemory::new(memory));
        Ok(ShardDirectoryStore::open(&slots).to_directory())
    }

    fn maintenance_queue_storage_snapshot_from_projection(
        projection: GraphStoreMaintenanceQueueStorageProjection,
    ) -> crate::low_level::GraphMaintenanceQueueStorageSnapshot {
        crate::low_level::GraphMaintenanceQueueStorageSnapshot {
            logical_len_bytes: projection.logical_len_bytes,
            queue_len: projection.queue_len,
            format_version: projection.format_version,
            checksum_valid: projection.checksum_valid,
        }
    }

    /// Bundles an existing region manager and graph runtime into one facade.
    pub fn new(manager: Rc<RefCell<RegionManager>>, memory: Rc<M>, graph: GraphRuntime) -> Self {
        let property_slots =
            GraphStoreMemorySlots::for_root_memory(RcGraphMemory(Rc::clone(&memory)));
        let node_property_store = open_fixed_slot_node_property_store(&property_slots);
        let edge_property_store = open_fixed_slot_edge_property_store(&property_slots);
        let property_equality_map = open_fixed_slot_property_equality_map(&property_slots);
        let mut store = Self {
            manager,
            memory,
            property_slots,
            graph,
            node_property_store,
            edge_property_store,
            node_property_index: PropertyIndex::new(64),
            edge_property_index: PropertyIndex::new(64),
            property_equality_map,
            property_index_dirty: false,
            node_property_store_dirty: false,
            edge_property_store_dirty: false,
            last_write_event: None,
            write_history: Vec::new(),
            production_metrics: GraphStoreProductionMetrics::default(),
            shard_canister_directory: ShardCanisterDirectory::default(),
        };
        store.sync_shadow_property_state_from_fixed_slots();
        store
    }

    /// Assembles a facade after hydration when property maps are already opened on fixed slots.
    fn assembled_after_property_load(args: AssembledAfterPropertyLoadArgs<M>) -> Self {
        Self {
            manager: args.manager,
            memory: args.memory,
            property_slots: args.property_slots,
            graph: args.graph,
            node_property_store: args.node_property_store,
            edge_property_store: args.edge_property_store,
            node_property_index: args.node_property_index,
            edge_property_index: args.edge_property_index,
            property_equality_map: args.property_equality_map,
            property_index_dirty: args.property_index_dirty,
            node_property_store_dirty: false,
            edge_property_store_dirty: false,
            last_write_event: None,
            write_history: Vec::new(),
            production_metrics: GraphStoreProductionMetrics::default(),
            shard_canister_directory: args.shard_canister_directory,
        }
    }

    /// Bootstraps one empty graph with the default bucket granularity.
    pub fn bootstrap_empty(memory: M) -> GraphStoreResult<Self> {
        Self::bootstrap_empty_with_bucket_size(BucketSizeInPages::DEFAULT, memory)
    }

    /// Bootstraps one empty graph with an explicit bucket granularity.
    pub fn bootstrap_empty_with_bucket_size(
        bucket_size_in_pages: BucketSizeInPages,
        memory: M,
    ) -> GraphStoreResult<Self> {
        Self::bootstrap_empty_with_bucket_size_using_shared_memory(
            bucket_size_in_pages,
            Rc::new(memory),
        )
    }

    /// Like [`Self::bootstrap_empty_with_bucket_size`], but reuses one shared [`Rc<M>`].
    pub fn bootstrap_empty_with_bucket_size_using_shared_memory(
        bucket_size_in_pages: BucketSizeInPages,
        mem_rc: Rc<M>,
    ) -> GraphStoreResult<Self> {
        let mgr_rc = Rc::new(RefCell::new(Self::bootstrap_region_manager_skeleton(
            bucket_size_in_pages,
        )));
        let forward = ForwardSurfaceRuntime::without_overflow(
            forward_surface_from_layout(&mgr_rc.borrow().layout)?,
            Vec::new(),
        );
        let reverse = ReverseSurfaceRuntime::without_overflow(
            reverse_surface_from_layout(&mgr_rc.borrow().layout)?,
            Vec::new(),
        );
        let mut facade = GraphStore::new(
            Rc::clone(&mgr_rc),
            Rc::clone(&mem_rc),
            GraphRuntime::new_with_empty_sidecars(forward, reverse),
        );
        facade.try_write_all_to_stable_memory(mem_rc.as_ref())?;
        Ok(facade)
    }

    /// Like [`Self::bootstrap_empty_with_bucket_size_using_shared_memory`]; legacy alias.
    pub fn bootstrap_empty_with_bucket_size_using_memory_rc(
        bucket_size_in_pages: BucketSizeInPages,
        mem_rc: Rc<M>,
    ) -> GraphStoreResult<Self> {
        Self::bootstrap_empty_with_bucket_size_using_shared_memory(bucket_size_in_pages, mem_rc)
    }

    /// Creates one facade from already-hydrated directional runtimes.
    pub fn from_hydrated_runtimes(
        manager: Rc<RefCell<RegionManager>>,
        memory: Rc<M>,
        runtimes: HydratedSurfaceRuntimes,
    ) -> Self {
        let mut graph = GraphRuntime::new_with_empty_sidecars(runtimes.forward, runtimes.reverse);
        let _ = graph.sync_base_segment_capacities_from_manager(&manager.borrow());
        Self::new(manager, memory, graph)
    }

    /// Creates one facade from hydrated runtimes and an explicit insert policy.
    pub fn from_hydrated_runtimes_with_insert_policy(
        manager: Rc<RefCell<RegionManager>>,
        memory: Rc<M>,
        runtimes: HydratedSurfaceRuntimes,
        insert_policy: GraphInsertPolicy,
    ) -> Self {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            runtimes.forward,
            runtimes.reverse,
            insert_policy,
        );
        let _ = graph.sync_base_segment_capacities_from_manager(&manager.borrow());
        Self::new(manager, memory, graph)
    }

    /// Hydrates from graph backing by reading the tail [`crate::low_level::pma_stable_root`] footer.
    pub fn hydrate_from_graph_stable_memory(memory: M) -> GraphStoreResult<Self> {
        let manager = crate::low_level::decode_region_manager_for_hydrate(&memory)?;
        Self::hydrate_from_stable_memory(manager, memory)
    }

    /// Hydrates forward/reverse runtimes from stable memory and builds a facade.
    ///
    /// The locator sidecar starts empty. Callers that already know the
    /// canonical forward-side semantic edge ids can repopulate it after
    /// hydration using the lower-level sidecar helpers.
    ///
    /// Property indices are loaded through [`PropertyIndexStorageImage::try_from_sectioned_parts`],
    /// which normalizes an **empty on-disk logical snapshot** against non-empty node stores so
    /// in-memory `node_property_index` / `edge_property_index` match persisted pages.
    pub fn hydrate_from_stable_memory(manager: RegionManager, memory: M) -> GraphStoreResult<Self> {
        let mgr_rc = Rc::new(RefCell::new(manager));
        let mem_rc = Rc::new(memory);
        let property_slots =
            GraphStoreMemorySlots::for_root_memory(RcGraphMemory(Rc::clone(&mem_rc)));
        let node_property_store = open_fixed_slot_node_property_store(&property_slots);
        let edge_property_store = open_fixed_slot_edge_property_store(&property_slots);
        let property_equality_map = open_fixed_slot_property_equality_map(&property_slots);
        let runtimes =
            hydrate_surface_runtimes_from_stable_memory(&mgr_rc.borrow(), mem_rc.as_ref())?;

        let mut graph = GraphRuntime::new_with_empty_sidecars(runtimes.forward, runtimes.reverse);
        let _ = graph.sync_base_segment_capacities_from_manager(&mgr_rc.borrow());

        let shard_canister_directory = Self::load_shard_canister_directory_from_stable_memory(
            &mgr_rc.borrow(),
            mem_rc.as_ref(),
        )?;
        graph.validate_shard_canister_slots(shard_canister_directory.len())?;

        let node_property_index = PropertyIndex::new(64);
        let edge_property_index = PropertyIndex::new(64);
        let property_index_dirty = false;

        let mut facade = Self::assembled_after_property_load(AssembledAfterPropertyLoadArgs {
            manager: mgr_rc,
            memory: mem_rc,
            property_slots,
            graph,
            node_property_store,
            edge_property_store,
            node_property_index,
            edge_property_index,
            property_equality_map,
            property_index_dirty,
            shard_canister_directory,
        });

        facade.sync_shadow_property_state_from_fixed_slots();

        let maintenance_queue = Self::load_maintenance_queue_from_slot(&facade.property_slots)?;
        facade.graph.replace_maintenance_queue(maintenance_queue);
        Ok(facade)
    }

    /// Hydrates forward/reverse runtimes from stable memory with an explicit insert policy.
    pub fn hydrate_from_stable_memory_with_insert_policy(
        manager: RegionManager,
        memory: M,
        insert_policy: GraphInsertPolicy,
    ) -> GraphStoreResult<Self> {
        let mgr_rc = Rc::new(RefCell::new(manager));
        let mem_rc = Rc::new(memory);
        let property_slots =
            GraphStoreMemorySlots::for_root_memory(RcGraphMemory(Rc::clone(&mem_rc)));
        let node_property_store = open_fixed_slot_node_property_store(&property_slots);
        let edge_property_store = open_fixed_slot_edge_property_store(&property_slots);
        let property_equality_map = open_fixed_slot_property_equality_map(&property_slots);
        let runtimes =
            hydrate_surface_runtimes_from_stable_memory(&mgr_rc.borrow(), mem_rc.as_ref())?;

        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            runtimes.forward,
            runtimes.reverse,
            insert_policy,
        );
        let _ = graph.sync_base_segment_capacities_from_manager(&mgr_rc.borrow());

        let shard_canister_directory = Self::load_shard_canister_directory_from_stable_memory(
            &mgr_rc.borrow(),
            mem_rc.as_ref(),
        )?;
        graph.validate_shard_canister_slots(shard_canister_directory.len())?;

        let node_property_index = PropertyIndex::new(64);
        let edge_property_index = PropertyIndex::new(64);
        let property_index_dirty = false;

        let mut facade = Self::assembled_after_property_load(AssembledAfterPropertyLoadArgs {
            manager: mgr_rc,
            memory: mem_rc,
            property_slots,
            graph,
            node_property_store,
            edge_property_store,
            node_property_index,
            edge_property_index,
            property_equality_map,
            property_index_dirty,
            shard_canister_directory,
        });

        facade.sync_shadow_property_state_from_fixed_slots();

        let maintenance_queue = Self::load_maintenance_queue_from_slot(&facade.property_slots)?;
        facade.graph.replace_maintenance_queue(maintenance_queue);
        Ok(facade)
    }

    /// Hydrates one facade from stable memory using the facade-level result type.
    pub fn try_hydrate_from_stable_memory(
        manager: RegionManager,
        memory: M,
    ) -> GraphStoreResult<Self> {
        Self::hydrate_from_stable_memory(manager, memory)
    }

    /// Hydrates one facade with an explicit insert policy using the facade-level result type.
    pub fn try_hydrate_from_stable_memory_with_insert_policy(
        manager: RegionManager,
        memory: M,
        insert_policy: GraphInsertPolicy,
    ) -> GraphStoreResult<Self> {
        Self::hydrate_from_stable_memory_with_insert_policy(manager, memory, insert_policy)
    }

    /// Hydrates one facade and immediately rebuilds the canonical logical-locator sidecar.
    ///
    /// The caller must supply forward-surface vertex refs explicitly; semantic
    /// `NodeId` conversion belongs outside the facade.
    pub fn hydrate_from_stable_memory_with_logical_locator_sidecar(
        manager: RegionManager,
        memory: M,
        forward_vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> GraphStoreResult<Self> {
        let mut facade = Self::try_hydrate_from_stable_memory(manager, memory)?;
        facade.try_rebuild_logical_locator_sidecar(
            forward_vertex_refs,
            forward_base_edge_ids_by_ordinal,
        )?;
        Ok(facade)
    }

    /// Hydrates one facade with an explicit insert policy and immediately rebuilds
    /// the canonical logical-locator sidecar.
    ///
    /// The caller must supply forward-surface vertex refs explicitly; semantic
    /// `NodeId` conversion belongs outside the facade.
    pub fn hydrate_from_stable_memory_with_insert_policy_and_logical_locator_sidecar(
        manager: RegionManager,
        memory: M,
        insert_policy: GraphInsertPolicy,
        forward_vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> GraphStoreResult<Self> {
        let mut facade = Self::try_hydrate_from_stable_memory_with_insert_policy(
            manager,
            memory,
            insert_policy,
        )?;
        facade.try_rebuild_logical_locator_sidecar(
            forward_vertex_refs,
            forward_base_edge_ids_by_ordinal,
        )?;
        Ok(facade)
    }

    /// Returns the region-manager metadata.
    pub fn manager(&self) -> Ref<'_, RegionManager> {
        self.manager.borrow()
    }

    /// Returns mutable access to the region-manager metadata.
    pub fn manager_mut(&mut self) -> RefMut<'_, RegionManager> {
        self.manager.borrow_mut()
    }

    /// Returns the graph runtime.
    pub const fn graph(&self) -> &GraphRuntime {
        &self.graph
    }

    /// Returns mutable access to the graph runtime.
    pub fn graph_mut(&mut self) -> &mut GraphRuntime {
        &mut self.graph
    }

    pub fn resolve_forward_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.graph
            .forward
            .resolve_logical_edge_slot(vertex_ref, ordinal, locator)
    }

    pub fn resolve_reverse_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.graph
            .reverse
            .resolve_logical_edge_slot(vertex_ref, ordinal, locator)
    }

    pub fn choose_insert_decision_with_incoming_live_entries(
        &self,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        incoming_live_entries: usize,
    ) -> Option<GraphInsertDecision> {
        let incoming_live_entries = u32::try_from(incoming_live_entries).ok()?;
        self.graph
            .choose_insert_decision_with_incoming_live_entries(
                src_vertex_ref,
                src_ordinal,
                dst_vertex_ref,
                dst_ordinal,
                incoming_live_entries,
            )
    }

    pub fn plan_local_rebalance(
        &self,
        plan: GraphRebalancePlan,
    ) -> Option<GraphLocalRebalancePlan> {
        self.graph.plan_local_rebalance(plan)
    }

    pub fn build_local_rebalance_delta(
        &self,
        plan: GraphLocalRebalancePlan,
    ) -> Option<GraphLocalRebalanceDelta> {
        self.graph.build_local_rebalance_delta(plan)
    }

    /// Returns immutable access to the stable node property map.
    pub fn node_property_store(&self) -> &GraphStoreNodePropertyMap<M> {
        &self.node_property_store
    }

    /// Returns mutable access to the stable node property map.
    pub fn node_property_store_mut(&mut self) -> &mut GraphStoreNodePropertyMap<M> {
        &mut self.node_property_store
    }

    /// Returns immutable access to the stable edge property map.
    pub fn edge_property_store(&self) -> &GraphStoreEdgePropertyMap<M> {
        &self.edge_property_store
    }

    /// Returns mutable access to the stable edge property map.
    pub fn edge_property_store_mut(&mut self) -> &mut GraphStoreEdgePropertyMap<M> {
        &mut self.edge_property_store
    }

    /// Returns the latest node properties for one semantic node id.
    pub fn scan_node_properties(&self, node_id: NodeId) -> PropertyMap {
        btree_scan_entity(
            &self.node_property_store,
            crate::PropertyEntityKind::Node,
            u64::from(node_id),
        )
    }

    /// Returns the latest edge properties for one semantic edge id.
    pub fn scan_edge_properties(&self, edge_id: EdgeId) -> PropertyMap {
        btree_scan_entity(
            &self.edge_property_store,
            crate::PropertyEntityKind::Edge,
            edge_id,
        )
    }

    /// Latest node properties for many ids in one btree scan.
    pub fn scan_node_properties_batch(&self, node_ids: &[NodeId]) -> BTreeMap<NodeId, PropertyMap> {
        let id_set: BTreeSet<u64> = node_ids.iter().map(|n| u64::from(*n)).collect();
        let by_u64 = btree_scan_entities(
            &self.node_property_store,
            crate::PropertyEntityKind::Node,
            &id_set,
        );
        by_u64
            .into_iter()
            .filter_map(|(u, m)| NodeId::try_from(u).ok().map(|id| (id, m)))
            .collect()
    }

    pub fn scan_node_properties_batch_subset(
        &self,
        node_ids: &[NodeId],
        property_names: &BTreeSet<String>,
    ) -> BTreeMap<NodeId, PropertyMap> {
        if node_ids.is_empty() {
            return BTreeMap::new();
        }
        let id_set: BTreeSet<u64> = node_ids.iter().map(|n| u64::from(*n)).collect();
        let by_u64 = btree_scan_entities_property_subset(
            &self.node_property_store,
            crate::PropertyEntityKind::Node,
            &id_set,
            property_names,
        );
        node_ids
            .iter()
            .map(|&id| {
                let u = u64::from(id);
                let props = by_u64.get(&u).cloned().unwrap_or_default();
                (id, props)
            })
            .collect()
    }

    pub fn scan_edge_properties_batch_subset(
        &self,
        edge_ids: &[EdgeId],
        property_names: &BTreeSet<String>,
    ) -> BTreeMap<EdgeId, PropertyMap> {
        if edge_ids.is_empty() {
            return BTreeMap::new();
        }
        let id_set: BTreeSet<u64> = edge_ids.iter().copied().collect();
        let by_u64 = btree_scan_entities_property_subset(
            &self.edge_property_store,
            crate::PropertyEntityKind::Edge,
            &id_set,
            property_names,
        );
        edge_ids
            .iter()
            .map(|&id| {
                let props = by_u64.get(&id).cloned().unwrap_or_default();
                (id, props)
            })
            .collect()
    }

    pub fn get_node_property_value(&self, node_id: NodeId, property: &str) -> Option<Value> {
        btree_get_node_property(&self.node_property_store, node_id, property)
    }

    pub fn get_edge_property_value(&self, edge_id: EdgeId, property: &str) -> Option<Value> {
        btree_get_edge_property(&self.edge_property_store, edge_id, property)
    }

    pub fn distinct_node_property_names(&self) -> BTreeSet<String> {
        btree_distinct_property_names(&self.node_property_store)
    }

    pub fn distinct_edge_property_names(&self) -> BTreeSet<String> {
        btree_distinct_property_names(&self.edge_property_store)
    }

    /// Returns node ids matching one exact equality property predicate.
    pub fn scan_node_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<NodeId> {
        let encoded_value = value
            .to_binary_bytes()
            .expect("Value must encode to binary bytes");
        self.node_property_index
            .scan_value_prefix(
                PropertyIndexEntityKind::VertexNode,
                property,
                &encoded_value,
            )
            .into_iter()
            .filter_map(|(key, _)| NodeId::try_from(key.entity_id).ok())
            .collect()
    }

    /// Returns node ids that have any binding for the given property name.
    pub fn scan_node_ids_by_property(&self, property: &str) -> Vec<NodeId> {
        self.node_property_index
            .scan_property_prefix(PropertyIndexEntityKind::VertexNode, property)
            .into_iter()
            .filter_map(|(key, _)| NodeId::try_from(key.entity_id).ok())
            .collect()
    }

    /// Returns edge ids that have any binding for the given property name.
    pub fn scan_edge_ids_by_property(&self, property: &str) -> Vec<EdgeId> {
        self.edge_property_index
            .scan_property_prefix(PropertyIndexEntityKind::VertexEdge, property)
            .into_iter()
            .map(|(key, _)| key.entity_id)
            .collect()
    }

    /// Returns edge ids matching one exact equality property predicate.
    pub fn scan_edge_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<EdgeId> {
        let encoded_value = value
            .to_binary_bytes()
            .expect("Value must encode to binary bytes");
        self.edge_property_index
            .scan_value_prefix(
                PropertyIndexEntityKind::VertexEdge,
                property,
                &encoded_value,
            )
            .into_iter()
            .map(|(key, _)| key.entity_id)
            .collect()
    }

    /// Reads node ids matching one exact equality predicate directly from stable memory.
    pub fn try_scan_node_ids_by_property_eq_from_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
        value: &Value,
    ) -> Result<Vec<NodeId>, PropertyIndexError> {
        let slots = GraphStoreMemorySlots::for_root_memory(BorrowedMemory::new(memory));
        let map = open_fixed_slot_property_equality_map(&slots);
        let node_index = snapshot_from_equality_any_memory(&map, 64).node_index;
        let encoded_value = value
            .to_binary_bytes()
            .expect("Value must encode to binary bytes");
        Ok(node_index
            .scan_value_prefix(
                PropertyIndexEntityKind::VertexNode,
                property,
                &encoded_value,
            )
            .into_iter()
            .filter_map(|(key, _)| NodeId::try_from(key.entity_id).ok())
            .collect())
    }

    /// Reads node ids that have any binding for the given property directly from stable memory.
    pub fn try_scan_node_ids_by_property_from_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
    ) -> Result<Vec<NodeId>, PropertyIndexError> {
        let slots = GraphStoreMemorySlots::for_root_memory(BorrowedMemory::new(memory));
        let map = open_fixed_slot_property_equality_map(&slots);
        let node_index = snapshot_from_equality_any_memory(&map, 64).node_index;
        Ok(node_index
            .scan_property_prefix(PropertyIndexEntityKind::VertexNode, property)
            .into_iter()
            .filter_map(|(key, _)| NodeId::try_from(key.entity_id).ok())
            .collect())
    }

    /// Reads edge ids matching one exact equality predicate directly from stable memory.
    pub fn try_scan_edge_ids_by_property_eq_from_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
        value: &Value,
    ) -> Result<Vec<EdgeId>, PropertyIndexError> {
        let slots = GraphStoreMemorySlots::for_root_memory(BorrowedMemory::new(memory));
        let map = open_fixed_slot_property_equality_map(&slots);
        let edge_index = snapshot_from_equality_any_memory(&map, 64).edge_index;
        let encoded_value = value
            .to_binary_bytes()
            .expect("Value must encode to binary bytes");
        Ok(edge_index
            .scan_value_prefix(
                PropertyIndexEntityKind::VertexEdge,
                property,
                &encoded_value,
            )
            .into_iter()
            .map(|(key, _)| key.entity_id)
            .collect())
    }

    /// Reads edge ids that have any binding for the given property directly from stable memory.
    pub fn try_scan_edge_ids_by_property_from_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
    ) -> Result<Vec<EdgeId>, PropertyIndexError> {
        let slots = GraphStoreMemorySlots::for_root_memory(BorrowedMemory::new(memory));
        let map = open_fixed_slot_property_equality_map(&slots);
        let edge_index = snapshot_from_equality_any_memory(&map, 64).edge_index;
        Ok(edge_index
            .scan_property_prefix(PropertyIndexEntityKind::VertexEdge, property)
            .into_iter()
            .map(|(key, _)| key.entity_id)
            .collect())
    }

    /// Reads one forward-surface vertex entry directly from stable memory by logical ordinal.
    pub fn try_read_forward_vertex_entry_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Result<Option<VertexEntry>, HydrationError> {
        read_vertex_entry_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            ordinal,
        )
    }

    /// Reads one reverse-surface vertex entry directly from stable memory by logical ordinal.
    pub fn try_read_reverse_vertex_entry_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Result<Option<VertexEntry>, HydrationError> {
        read_vertex_entry_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            ordinal,
        )
    }

    /// Reads one forward-surface vertex entry directly from stable memory by packed vertex ref.
    pub fn try_read_forward_vertex_entry_by_ref_from_stable_memory(
        &self,
        memory: &impl Memory,
        vertex_ref: VertexRef,
    ) -> Result<Option<VertexEntry>, HydrationError> {
        read_vertex_entry_by_ref_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            vertex_ref,
        )
    }

    /// Reads one reverse-surface vertex entry directly from stable memory by packed vertex ref.
    pub fn try_read_reverse_vertex_entry_by_ref_from_stable_memory(
        &self,
        memory: &impl Memory,
        vertex_ref: VertexRef,
    ) -> Result<Option<VertexEntry>, HydrationError> {
        read_vertex_entry_by_ref_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            vertex_ref,
        )
    }

    /// Reads the reserved base-span length for one forward vertex directly from stable memory.
    pub fn try_read_forward_vertex_reserved_span_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Result<Option<u64>, HydrationError> {
        read_vertex_reserved_span_len_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            ordinal,
        )
    }

    /// Reads the reserved base-span length for one reverse vertex directly from stable memory.
    pub fn try_read_reverse_vertex_reserved_span_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Result<Option<u64>, HydrationError> {
        read_vertex_reserved_span_len_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            ordinal,
        )
    }

    /// Reads one contiguous edge slice directly from the forward edge table by packed edge ref.
    pub fn try_read_forward_edge_entries_by_ref_from_stable_memory(
        &self,
        memory: &impl Memory,
        edge_ref: crate::low_level::EdgeRef,
        count: usize,
    ) -> Result<Vec<EdgeEntry>, HydrationError> {
        read_edge_entries_by_ref_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardEdgeEntries,
            edge_ref,
            count,
        )
    }

    /// Reads one contiguous edge slice directly from the reverse edge table by packed edge ref.
    pub fn try_read_reverse_edge_entries_by_ref_from_stable_memory(
        &self,
        memory: &impl Memory,
        edge_ref: crate::low_level::EdgeRef,
        count: usize,
    ) -> Result<Vec<EdgeEntry>, HydrationError> {
        read_edge_entries_by_ref_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseEdgeEntries,
            edge_ref,
            count,
        )
    }

    /// Reads live base entries for one forward vertex directly from stable memory.
    pub fn try_read_forward_vertex_base_entries_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Result<Option<Vec<EdgeEntry>>, HydrationError> {
        read_vertex_base_entries_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            ordinal,
        )
    }

    /// Reads live base entries for one reverse vertex directly from stable memory.
    pub fn try_read_reverse_vertex_base_entries_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Result<Option<Vec<EdgeEntry>>, HydrationError> {
        read_vertex_base_entries_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            ordinal,
        )
    }

    /// Resolves one forward vertex-local base entry to its packed edge ref directly from stable memory.
    pub fn try_read_forward_vertex_base_edge_ref_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
        logical_index: usize,
    ) -> Result<Option<crate::low_level::EdgeRef>, HydrationError> {
        read_vertex_base_edge_ref_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            ordinal,
            logical_index,
        )
    }

    /// Resolves one reverse vertex-local base entry to its packed edge ref directly from stable memory.
    pub fn try_read_reverse_vertex_base_edge_ref_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
        logical_index: usize,
    ) -> Result<Option<crate::low_level::EdgeRef>, HydrationError> {
        read_vertex_base_edge_ref_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            ordinal,
            logical_index,
        )
    }

    /// Reads one forward live base entry directly from stable memory by logical base index.
    pub fn try_read_forward_vertex_base_entry_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
        logical_index: usize,
    ) -> Result<Option<EdgeEntry>, HydrationError> {
        read_vertex_base_entry_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            ordinal,
            logical_index,
        )
    }

    /// Reads one reverse live base entry directly from stable memory by logical base index.
    pub fn try_read_reverse_vertex_base_entry_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
        logical_index: usize,
    ) -> Result<Option<EdgeEntry>, HydrationError> {
        read_vertex_base_entry_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            ordinal,
            logical_index,
        )
    }

    /// Reads the full reserved base span for one forward vertex directly from stable memory.
    pub fn try_read_forward_vertex_reserved_base_entries_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Result<Option<Vec<EdgeEntry>>, HydrationError> {
        read_vertex_reserved_base_entries_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            ordinal,
        )
    }

    /// Reads the full reserved base span for one reverse vertex directly from stable memory.
    pub fn try_read_reverse_vertex_reserved_base_entries_from_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Result<Option<Vec<EdgeEntry>>, HydrationError> {
        read_vertex_reserved_base_entries_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            ordinal,
        )
    }

    /// Returns one forward-surface vertex entry, preferring stable memory when the vertex table is clean.
    pub fn read_forward_vertex_entry_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Option<VertexEntry> {
        if !self.graph.forward.0.dirty_regions().vertex_table
            && let Ok(entry) =
                self.try_read_forward_vertex_entry_from_stable_memory(memory, ordinal)
        {
            return entry;
        }
        self.graph.forward.0.vertex_entry(ordinal)
    }

    /// Returns one reverse-surface vertex entry, preferring stable memory when the vertex table is clean.
    pub fn read_reverse_vertex_entry_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        ordinal: usize,
    ) -> Option<VertexEntry> {
        if !self.graph.reverse.0.dirty_regions().vertex_table
            && let Ok(entry) =
                self.try_read_reverse_vertex_entry_from_stable_memory(memory, ordinal)
        {
            return entry;
        }
        self.graph.reverse.0.vertex_entry(ordinal)
    }

    /// Reads forward-surface vertex entries directly from stable memory over one logical ordinal range.
    pub fn try_read_forward_vertex_entries_from_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
    ) -> Result<Vec<VertexEntry>, HydrationError> {
        read_vertex_entries_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            start_ordinal,
            count,
        )
    }

    /// Reads reverse-surface vertex entries directly from stable memory over one logical ordinal range.
    pub fn try_read_reverse_vertex_entries_from_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
    ) -> Result<Vec<VertexEntry>, HydrationError> {
        read_vertex_entries_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            start_ordinal,
            count,
        )
    }

    /// Returns forward-surface vertex entries, preferring stable memory when the vertex table is clean.
    pub fn read_forward_vertex_entries_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
    ) -> Vec<VertexEntry> {
        if !self.graph.forward.0.dirty_regions().vertex_table
            && let Ok(entries) = self.try_read_forward_vertex_entries_from_stable_memory(
                memory,
                start_ordinal,
                count,
            )
        {
            return entries;
        }
        self.graph
            .forward
            .0
            .vertices
            .iter()
            .skip(start_ordinal)
            .take(count)
            .copied()
            .collect()
    }

    /// Returns reverse-surface vertex entries, preferring stable memory when the vertex table is clean.
    pub fn read_reverse_vertex_entries_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
    ) -> Vec<VertexEntry> {
        if !self.graph.reverse.0.dirty_regions().vertex_table
            && let Ok(entries) = self.try_read_reverse_vertex_entries_from_stable_memory(
                memory,
                start_ordinal,
                count,
            )
        {
            return entries;
        }
        self.graph
            .reverse
            .0
            .vertices
            .iter()
            .skip(start_ordinal)
            .take(count)
            .copied()
            .collect()
    }

    /// Summarizes one forward-surface vertex window directly from stable memory.
    pub fn try_summarize_forward_vertex_window_from_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
    ) -> Result<Option<SurfaceVertexWindowSummary>, HydrationError> {
        summarize_vertex_window_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            start_ordinal,
            count,
        )
    }

    /// Summarizes one reverse-surface vertex window directly from stable memory.
    pub fn try_summarize_reverse_vertex_window_from_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
    ) -> Result<Option<SurfaceVertexWindowSummary>, HydrationError> {
        summarize_vertex_window_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            start_ordinal,
            count,
        )
    }

    /// Summarizes one forward-surface vertex window, preferring stable memory when the vertex table is clean.
    pub fn summarize_forward_vertex_window_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
    ) -> Option<SurfaceVertexWindowSummary> {
        if !self.graph.forward.0.dirty_regions().vertex_table
            && let Ok(summary) = self.try_summarize_forward_vertex_window_from_stable_memory(
                memory,
                start_ordinal,
                count,
            )
        {
            return summary;
        }
        self.graph
            .forward
            .0
            .summarize_vertex_window(start_ordinal, start_ordinal.saturating_add(count))
    }

    /// Summarizes one reverse-surface vertex window, preferring stable memory when the vertex table is clean.
    pub fn summarize_reverse_vertex_window_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
    ) -> Option<SurfaceVertexWindowSummary> {
        if !self.graph.reverse.0.dirty_regions().vertex_table
            && let Ok(summary) = self.try_summarize_reverse_vertex_window_from_stable_memory(
                memory,
                start_ordinal,
                count,
            )
        {
            return summary;
        }
        self.graph
            .reverse
            .0
            .summarize_vertex_window(start_ordinal, start_ordinal.saturating_add(count))
    }

    /// Estimates a lower-bound forward-surface reserve hint directly from stable memory.
    pub fn try_estimate_forward_vertex_window_reserve_hint_from_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
        anchor_live_degree_after_rebalance: u32,
        incoming_live_entries: u32,
    ) -> Result<Option<SurfaceVertexWindowReserveHint>, HydrationError> {
        estimate_vertex_window_reserve_hint_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ForwardVertexTable,
            start_ordinal,
            count,
            (self.graph.insert_policy, anchor_live_degree_after_rebalance),
            incoming_live_entries,
        )
    }

    /// Estimates a lower-bound reverse-surface reserve hint directly from stable memory.
    pub fn try_estimate_reverse_vertex_window_reserve_hint_from_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
        anchor_live_degree_after_rebalance: u32,
        incoming_live_entries: u32,
    ) -> Result<Option<SurfaceVertexWindowReserveHint>, HydrationError> {
        estimate_vertex_window_reserve_hint_from_stable_memory(
            &self.manager.borrow(),
            memory,
            RegionKind::ReverseVertexTable,
            start_ordinal,
            count,
            (self.graph.insert_policy, anchor_live_degree_after_rebalance),
            incoming_live_entries,
        )
    }

    /// Estimates a lower-bound forward-surface reserve hint, preferring stable memory when the vertex table is clean.
    pub fn estimate_forward_vertex_window_reserve_hint_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
        anchor_live_degree_after_rebalance: u32,
        incoming_live_entries: u32,
    ) -> Option<SurfaceVertexWindowReserveHint> {
        if !self.graph.forward.0.dirty_regions().vertex_table
            && let Ok(hint) = self
                .try_estimate_forward_vertex_window_reserve_hint_from_stable_memory(
                    memory,
                    start_ordinal,
                    count,
                    anchor_live_degree_after_rebalance,
                    incoming_live_entries,
                )
        {
            return hint;
        }
        self.graph
            .forward
            .0
            .summarize_vertex_window(start_ordinal, start_ordinal.saturating_add(count))
            .and_then(|summary| {
                self.graph
                    .insert_policy
                    .estimate_vertex_window_reserve_hint(
                        summary,
                        anchor_live_degree_after_rebalance,
                        incoming_live_entries,
                    )
            })
    }

    /// Estimates a lower-bound reverse-surface reserve hint, preferring stable memory when the vertex table is clean.
    pub fn estimate_reverse_vertex_window_reserve_hint_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        start_ordinal: usize,
        count: usize,
        anchor_live_degree_after_rebalance: u32,
        incoming_live_entries: u32,
    ) -> Option<SurfaceVertexWindowReserveHint> {
        if !self.graph.reverse.0.dirty_regions().vertex_table
            && let Ok(hint) = self
                .try_estimate_reverse_vertex_window_reserve_hint_from_stable_memory(
                    memory,
                    start_ordinal,
                    count,
                    anchor_live_degree_after_rebalance,
                    incoming_live_entries,
                )
        {
            return hint;
        }
        self.graph
            .reverse
            .0
            .summarize_vertex_window(start_ordinal, start_ordinal.saturating_add(count))
            .and_then(|summary| {
                self.graph
                    .insert_policy
                    .estimate_vertex_window_reserve_hint(
                        summary,
                        anchor_live_degree_after_rebalance,
                        incoming_live_entries,
                    )
            })
    }

    /// Returns node ids matching one equality predicate, preferring stable-memory direct scan when clean.
    pub fn scan_node_ids_by_property_eq_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
        value: &Value,
    ) -> Vec<NodeId> {
        let _ = memory;
        let started = Instant::now();
        let ids = self.scan_node_ids_by_property_eq(property, value);
        self.production_metrics
            .record_node_eq_scan_nanos(started.elapsed().as_nanos() as u64);
        ids
    }

    /// Returns node ids that have any binding for the given property, preferring stable-memory direct scan when clean.
    pub fn scan_node_ids_by_property_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
    ) -> Vec<NodeId> {
        let _ = memory;
        self.scan_node_ids_by_property(property)
    }

    /// Returns edge ids matching one equality predicate, preferring stable-memory direct scan when clean.
    pub fn scan_edge_ids_by_property_eq_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
        value: &Value,
    ) -> Vec<EdgeId> {
        let _ = memory;
        let started = Instant::now();
        let ids = self.scan_edge_ids_by_property_eq(property, value);
        self.production_metrics
            .record_edge_eq_scan_nanos(started.elapsed().as_nanos() as u64);
        ids
    }

    /// Returns edge ids that have any binding for the given property, preferring stable-memory direct scan when clean.
    pub fn scan_edge_ids_by_property_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
    ) -> Vec<EdgeId> {
        let _ = memory;
        self.scan_edge_ids_by_property(property)
    }

    /// Returns whether node-side property state has unflushed changes.
    pub const fn node_property_store_is_dirty(&self) -> bool {
        false
    }

    /// Returns whether edge-side property state has unflushed changes.
    pub const fn edge_property_store_is_dirty(&self) -> bool {
        false
    }

    /// Returns a snapshot of production-facing property/index metrics.
    pub fn production_metrics_snapshot(&self) -> GraphStoreProductionMetricsSnapshot {
        self.production_metrics.snapshot()
    }

    /// Replaces the canonical logical-locator sidecar by rebuilding it from externally
    /// supplied forward-surface vertex refs plus semantic edge ids.
    pub fn rebuild_logical_locator_sidecar(
        &mut self,
        forward_vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<()> {
        let sidecar = self
            .graph
            .forward
            .0
            .build_logical_locator_sidecar_from_vertex_base_ids(
                forward_vertex_refs,
                forward_base_edge_ids_by_ordinal,
            )?;
        self.graph.replace_logical_locator_sidecar(sidecar);
        Some(())
    }

    /// Rebuilds the canonical logical-locator sidecar using the facade-level result type.
    pub fn try_rebuild_logical_locator_sidecar(
        &mut self,
        forward_vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> GraphStoreResult<()> {
        self.rebuild_logical_locator_sidecar(forward_vertex_refs, forward_base_edge_ids_by_ordinal)
            .ok_or(GraphStoreError::InvalidLocatorInputs)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GraphStore, GraphStoreAppendVertexWriteSummary, GraphStoreAppendVerticesWriteSummary,
        GraphStoreBootstrapEdgeProjection, GraphStoreBootstrapEdgeWriteSummary,
        GraphStoreBootstrapGraphProjection, GraphStoreBootstrapGraphWriteSummary,
        GraphStoreBootstrapVerticesProjection, GraphStoreEdgeLogicalLocatorMapping,
        GraphStoreEdgeWriteOperation, GraphStoreEnsureCapacityProjection, GraphStoreError,
        GraphStoreFacadeWriteEvent, GraphStoreInsertEdgeProjection,
        GraphStoreMaintenanceBatchProjection, GraphStoreMaintenanceCycleProjection,
        GraphStoreMaintenanceQueueAction, GraphStoreMaintenanceQueueStorageProjection,
        GraphStoreMutationWriteSummary, GraphStorePropertyIndexTouchedSections,
        GraphStoreRefreshedVertices, GraphStoreResult, GraphStoreService, GraphStoreStore,
        GraphStoreStoreAdapter, GraphStoreVertexOrdinalMapping, GraphStoreWriteEventProjection,
    };
    use crate::GraphInsertResult;
    use crate::VecMemory;
    use crate::adjacency::{BorrowedMemory, GraphStoreMemorySlots};
    use crate::low_level::GraphMutationPath;
    use crate::low_level::{
        BucketSizeInPages, EMPTY_LOG_OFFSET, EdgeEntry, EdgeIndex, EdgeMeta, EdgePairEndpoints,
        EdgePairLogicalLocators, EdgeRef, EdgeReplaceSpec, EdgeTombstoneSpec, GraphInsertPolicy,
        GraphMaintenanceWorkItem, HydratedSurfaceRuntimes, HydrationError, LogicalEdgeLocator,
        RebalanceInsertSpec, RebalancePrepareSpec, RegionKind, RegionManager, SurfaceBaseStorage,
        SurfaceKind, VertexEntry, WASM_PAGE_SIZE, encode_edge_entries, encode_label_index_region,
        encode_overflow_entries, encode_vertex_entries, forward_surface_from_layout,
        reverse_surface_from_layout, write_surface_runtimes_to_stable_memory,
    };
    use crate::observability::{project_facade_write_event, project_facade_write_history};
    use crate::property_index::{
        PropertyIndexEntityKind, PropertyIndexError, PropertyIndexNodeStoreMutationKind,
    };
    use crate::property_index::{
        open_fixed_slot_property_equality_map, snapshot_fixed_slot_property_equality_map,
    };
    use crate::property_store::{
        PropertyKey, PropertyStoreError, StoredPropertyValue,
        load_graph_property_stable_map_from_stable_memory, open_fixed_slot_edge_property_store,
        open_fixed_slot_node_property_store, sync_graph_property_store_v1_header_to_stable_memory,
    };
    use gleaph_gql::Value;
    use gleaph_graph_kernel::NodeId;
    use ic_stable_structures::Memory;
    use std::cell::RefCell;
    use std::rc::Rc;

    type TestPma = GraphStore<VecMemory>;

    fn assert_projected_history(
        events: &[GraphStoreFacadeWriteEvent],
        expected: Vec<GraphStoreWriteEventProjection>,
    ) {
        assert_eq!(project_facade_write_history(events), expected);
    }

    fn define_surface_regions(manager: &mut RegionManager, prefix: crate::low_level::SurfaceKind) {
        GraphStore::<VecMemory>::define_empty_surface_regions(manager, prefix);
    }

    fn define_property_regions(manager: &mut RegionManager) {
        GraphStore::<VecMemory>::define_empty_property_regions(manager);
    }

    fn seeded_manager_and_memory() -> (RegionManager, VecMemory) {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        define_surface_regions(&mut manager, crate::low_level::SurfaceKind::Forward);
        define_surface_regions(&mut manager, crate::low_level::SurfaceKind::Reverse);
        define_property_regions(&mut manager);

        let forward_vertices = vec![VertexEntry::new(
            EdgeIndex::new(0),
            1,
            crate::low_level::EMPTY_LOG_OFFSET,
        )];
        let forward_edges = vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))];
        let reverse_vertices = vec![VertexEntry::new(
            EdgeIndex::new(0),
            1,
            crate::low_level::EMPTY_LOG_OFFSET,
        )];
        let reverse_edges = vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))];

        manager
            .set_region_logical_len(
                RegionKind::ForwardVertexTable,
                encode_vertex_entries(&forward_vertices).len() as u64,
            )
            .unwrap();
        manager
            .set_region_logical_len(
                RegionKind::ForwardEdgeEntries,
                encode_edge_entries(&forward_edges).len() as u64,
            )
            .unwrap();
        manager
            .set_region_logical_len(
                RegionKind::ForwardLabelIndex,
                encode_label_index_region(&[], &[]).len() as u64,
            )
            .unwrap();
        manager
            .set_region_logical_len(
                RegionKind::ForwardSegmentLog,
                encode_overflow_entries(&[]).len() as u64,
            )
            .unwrap();
        manager
            .set_region_logical_len(
                RegionKind::ReverseVertexTable,
                encode_vertex_entries(&reverse_vertices).len() as u64,
            )
            .unwrap();
        manager
            .set_region_logical_len(
                RegionKind::ReverseEdgeEntries,
                encode_edge_entries(&reverse_edges).len() as u64,
            )
            .unwrap();
        manager
            .set_region_logical_len(
                RegionKind::ReverseLabelIndex,
                encode_label_index_region(&[], &[]).len() as u64,
            )
            .unwrap();
        manager
            .set_region_logical_len(
                RegionKind::ReverseSegmentLog,
                encode_overflow_entries(&[]).len() as u64,
            )
            .unwrap();
        let memory = VecMemory::default();
        let write_region =
            |manager: &RegionManager, memory: &VecMemory, kind: RegionKind, bytes: Vec<u8>| {
                let region = manager.layout.region(kind).unwrap();
                match region.storage_kind() {
                    crate::low_level::RegionStorageKind::Extent => {
                        let extent = manager.region_extent(kind).unwrap();
                        let required_pages =
                            (extent.addr.0 + u64::try_from(bytes.len()).unwrap()).div_ceil(65_536);
                        let current_pages = memory.size();
                        if required_pages > current_pages {
                            assert_eq!(
                                memory.grow(required_pages - current_pages),
                                i64::try_from(current_pages).unwrap()
                            );
                        }
                        if !bytes.is_empty() {
                            memory.write(extent.addr.0, &bytes);
                        }
                    }
                    crate::low_level::RegionStorageKind::BucketChain => {
                        let chain = manager.bucket_chain(kind).unwrap();
                        let head = manager.bucket_header(chain.head).unwrap();
                        let required_pages = (head.addr.0
                            + manager.bucket_size_bytes()
                            + u64::try_from(bytes.len().saturating_sub(1)).unwrap())
                        .div_ceil(65_536);
                        let current_pages = memory.size();
                        if required_pages > current_pages {
                            assert_eq!(
                                memory.grow(required_pages - current_pages),
                                i64::try_from(current_pages).unwrap()
                            );
                        }
                        if !bytes.is_empty() {
                            memory.write(head.addr.0, &bytes);
                        }
                    }
                }
            };

        write_region(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            encode_vertex_entries(&forward_vertices),
        );
        write_region(
            &manager,
            &memory,
            RegionKind::ForwardEdgeEntries,
            encode_edge_entries(&forward_edges),
        );
        write_region(
            &manager,
            &memory,
            RegionKind::ForwardLabelIndex,
            encode_label_index_region(&[], &[]),
        );
        write_region(
            &manager,
            &memory,
            RegionKind::ForwardSegmentLog,
            encode_overflow_entries(&[]),
        );
        write_region(
            &manager,
            &memory,
            RegionKind::ReverseVertexTable,
            encode_vertex_entries(&reverse_vertices),
        );
        write_region(
            &manager,
            &memory,
            RegionKind::ReverseEdgeEntries,
            encode_edge_entries(&reverse_edges),
        );
        write_region(
            &manager,
            &memory,
            RegionKind::ReverseLabelIndex,
            encode_label_index_region(&[], &[]),
        );
        write_region(
            &manager,
            &memory,
            RegionKind::ReverseSegmentLog,
            encode_overflow_entries(&[]),
        );
        sync_graph_property_store_v1_header_to_stable_memory(
            &mut manager,
            &memory,
            RegionKind::NodePropertyStore,
            0,
        )
        .unwrap();
        sync_graph_property_store_v1_header_to_stable_memory(
            &mut manager,
            &memory,
            RegionKind::EdgePropertyStore,
            0,
        )
        .unwrap();

        (manager, memory)
    }

    #[test]
    fn facade_hydrates_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        assert_eq!(facade.graph.forward.0.vertices.len(), 1);
        assert_eq!(facade.graph.forward.0.base_entries.len(), 1);
        assert_eq!(facade.graph.reverse.0.vertices.len(), 1);
        assert_eq!(facade.graph.reverse.0.base_entries.len(), 1);
    }

    #[test]
    fn facade_hydrate_syncs_fixed_slot_property_state_into_shadow_maps() {
        let (manager, memory) = seeded_manager_and_memory();
        let slots = GraphStoreMemorySlots::for_root_memory(BorrowedMemory::new(&memory));

        let mut node_store = open_fixed_slot_node_property_store(&slots);
        node_store.insert(
            PropertyKey::node(NodeId::from(11u8), "name"),
            StoredPropertyValue(Value::Text("alice".into())),
        );
        let mut edge_store = open_fixed_slot_edge_property_store(&slots);
        edge_store.insert(
            PropertyKey::edge(99, "weight"),
            StoredPropertyValue(Value::Int64(7)),
        );
        let mut equality = open_fixed_slot_property_equality_map(&slots);
        equality.insert(
            crate::property_index::PropertyIndexKey::node(
                NodeId::from(11u8),
                "name",
                Value::Text("alice".into()).to_binary_bytes().unwrap(),
            ),
            crate::property_index::PropertyIndexEntry::empty(),
        );

        let facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        assert_eq!(
            facade.scan_node_properties(NodeId::from(11u8)).get("name"),
            Some(&Value::Text("alice".into()))
        );
        assert_eq!(
            facade.scan_edge_properties(99).get("weight"),
            Some(&Value::Int64(7))
        );
        assert_eq!(
            facade
                .node_property_store()
                .get(&PropertyKey::node(NodeId::from(11u8), "name"))
                .map(|value| value.0.clone()),
            Some(Value::Text("alice".into()))
        );
        assert_eq!(facade.property_equality_map.len(), 1);
        assert!(
            facade
                .node_property_index
                .get(&crate::property_index::PropertyIndexKey::node(
                    NodeId::from(11u8),
                    "name",
                    Value::Text("alice".into()).to_binary_bytes().unwrap(),
                ))
                .is_some()
        );
    }

    #[test]
    fn surface_write_after_property_insert_does_not_invalidate_node_property_btree() {
        let mem_rc = Rc::new(VecMemory::default());
        let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
            BucketSizeInPages::new(1),
            Rc::clone(&mem_rc),
        )
        .unwrap();
        let node_id = NodeId::from(11u8);
        let payload = Value::Text("y".repeat((WASM_PAGE_SIZE as usize) + 512));
        let _ = facade.node_property_store_mut().insert(
            PropertyKey::node(node_id, "profile"),
            StoredPropertyValue(payload.clone()),
        );
        let runtimes = HydratedSurfaceRuntimes::new(
            facade.graph.forward.clone(),
            facade.graph.reverse.clone(),
        );
        write_surface_runtimes_to_stable_memory(
            &mut facade.manager.borrow_mut(),
            mem_rc.as_ref(),
            &runtimes,
        )
        .expect("surface write");
        assert_eq!(
            facade.get_node_property_value(node_id, "profile"),
            Some(payload)
        );
    }

    #[test]
    fn facade_from_hydrated_runtimes_syncs_explicit_segment_capacities() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        define_surface_regions(&mut manager, SurfaceKind::Forward);
        define_surface_regions(&mut manager, SurfaceKind::Reverse);
        define_property_regions(&mut manager);
        manager
            .register_edge_segment(
                RegionKind::ForwardEdgeEntries,
                crate::low_level::EdgeSegmentHeader::new(
                    7,
                    crate::low_level::ExtentId::new(70),
                    11,
                    0,
                    crate::low_level::EdgeSegmentState::Active,
                ),
            )
            .unwrap();
        manager
            .register_edge_segment(
                RegionKind::ReverseEdgeEntries,
                crate::low_level::EdgeSegmentHeader::new(
                    9,
                    crate::low_level::ExtentId::new(90),
                    13,
                    0,
                    crate::low_level::EdgeSegmentState::Active,
                ),
            )
            .unwrap();

        let runtimes = HydratedSurfaceRuntimes::new(
            crate::low_level::ForwardSurfaceRuntime::new(
                forward_surface_from_layout(&manager.layout).unwrap(),
                vec![VertexEntry::new(
                    EdgeIndex::from(EdgeRef::new(7, 0)),
                    0,
                    EMPTY_LOG_OFFSET,
                )],
                Vec::new(),
                Vec::new(),
            ),
            crate::low_level::ReverseSurfaceRuntime::new(
                reverse_surface_from_layout(&manager.layout).unwrap(),
                vec![VertexEntry::new(
                    EdgeIndex::from(EdgeRef::new(9, 0)),
                    0,
                    EMPTY_LOG_OFFSET,
                )],
                Vec::new(),
                Vec::new(),
            ),
        );

        let mgr_rc = Rc::new(RefCell::new(manager));
        let mem_rc = Rc::new(VecMemory::default());
        let facade = GraphStore::from_hydrated_runtimes(mgr_rc, mem_rc, runtimes);

        assert_eq!(
            facade.graph.forward.0.base_segment_slot_capacity(7),
            Some(11)
        );
        assert_eq!(
            facade.graph.reverse.0.base_segment_slot_capacity(9),
            Some(13)
        );
    }

    #[test]
    fn facade_persists_maintenance_queue_across_hydration() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(73u8);
        let dst = NodeId::from(74u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            maintenance_recent_epoch_window: 3,
            maintenance_recent_epoch_penalty: 100_000,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(74u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(75u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(73u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(76u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1009,
            EdgeEntry::new(
                NodeId::from(77u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1010,
            EdgeEntry::new(
                NodeId::from(73u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.record_recent_maintenance_window(0, 1, 10);

        assert_eq!(
            facade.rebuild_maintenance_queue_at_epoch(&[src.into(), dst.into()], 11),
            Some(2)
        );
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_refresh_and_write_dirty_to_stable_memory(mem_rc.as_ref())
            .expect("write dirty including queue");

        let hydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .unwrap();
        let queue = hydrated.maintenance_queue_projection();
        assert_eq!(queue.len(), 2);
        let src_item = queue
            .iter()
            .find(|item| item.vertex_ref == src.into())
            .expect("src queue item");
        assert_eq!(src_item.window_start_ordinal, 0);
        assert_eq!(src_item.window_end_ordinal_exclusive, 1);
        assert!(src_item.recent_maintenance_penalty > 0);
    }

    #[test]
    fn facade_can_read_maintenance_queue_directly_from_stable_memory() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(83u8);
        let dst = NodeId::from(84u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            maintenance_recent_epoch_window: 3,
            maintenance_recent_epoch_penalty: 100_000,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(84u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(85u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(83u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(86u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1013,
            EdgeEntry::new(
                NodeId::from(87u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1014,
            EdgeEntry::new(
                NodeId::from(83u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_refresh_and_write_dirty_to_stable_memory(mem_rc.as_ref())
            .expect("persist graph state");
        facade
            .rebuild_maintenance_queue_at_epoch_and_write(
                &[src.into(), dst.into()],
                11,
                mem_rc.as_ref(),
            )
            .expect("persist maintenance queue");

        let queue = facade
            .try_read_maintenance_queue_from_stable_memory(mem_rc.as_ref())
            .expect("read queue bytes");
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].start_ordinal, 0);

        let projection = facade
            .try_read_maintenance_queue_projection_from_stable_memory(mem_rc.as_ref())
            .expect("read queue projection");
        assert_eq!(projection.len(), 2);
        assert!(projection.iter().any(|item| item.vertex_ref == src.into()));

        let formatted = facade
            .try_format_maintenance_queue_from_stable_memory(mem_rc.as_ref())
            .expect("format queue");
        assert_eq!(formatted.len(), 2);
        assert!(
            formatted
                .iter()
                .any(|line| line.contains("maintenance-queue vertex=83")
                    && line.contains("window=(0, 1)"))
        );

        let storage = facade
            .try_read_maintenance_queue_storage_projection_from_stable_memory(mem_rc.as_ref())
            .expect("read queue storage projection");
        assert_eq!(
            storage.format_version,
            Some(TestPma::SERIALIZED_MAINTENANCE_QUEUE_VERSION)
        );
        assert_eq!(storage.queue_len, 2);
        assert_eq!(storage.checksum_valid, Some(true));
        assert!(storage.logical_len_bytes > 0);

        let formatted_storage = facade
            .try_format_maintenance_queue_storage_from_stable_memory(mem_rc.as_ref())
            .expect("format storage metadata");
        assert!(formatted_storage.contains("maintenance-queue-storage"));
        assert!(formatted_storage.contains("version="));
        assert!(formatted_storage.contains("checksum="));
    }

    #[test]
    fn facade_rebuild_maintenance_queue_and_write_persists_queue_without_graph_writeback() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(78u8);
        let dst = NodeId::from(79u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            maintenance_recent_epoch_window: 3,
            maintenance_recent_epoch_penalty: 100_000,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(79u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(80u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(78u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(81u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1011,
            EdgeEntry::new(
                NodeId::from(82u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1012,
            EdgeEntry::new(
                NodeId::from(78u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_refresh_and_write_dirty_to_stable_memory(mem_rc.as_ref())
            .expect("persist graph state");

        assert_eq!(
            facade
                .rebuild_maintenance_queue_at_epoch_and_write(
                    &[src.into(), dst.into()],
                    11,
                    mem_rc.as_ref(),
                )
                .expect("persist queue"),
            Some(2)
        );

        let hydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .unwrap();
        assert_eq!(hydrated.maintenance_queue_projection().len(), 2);
    }

    #[test]
    fn facade_rejects_premgq1_maintenance_queue_bytes() {
        let item = GraphMaintenanceWorkItem {
            vertex_ref: NodeId::from(88u8).into(),
            anchor_ordinal: 3,
            start_ordinal: 2,
            end_ordinal_exclusive: 5,
            priority_score: 42,
            last_maintenance_epoch: Some(9),
            recent_maintenance_penalty: 7,
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&u64::from(item.vertex_ref).to_le_bytes());
        bytes.extend_from_slice(&(item.anchor_ordinal as u64).to_le_bytes());
        bytes.extend_from_slice(&(item.start_ordinal as u64).to_le_bytes());
        bytes.extend_from_slice(&(item.end_ordinal_exclusive as u64).to_le_bytes());
        bytes.extend_from_slice(&item.priority_score.to_le_bytes());
        bytes.extend_from_slice(&item.last_maintenance_epoch.unwrap().to_le_bytes());
        bytes.extend_from_slice(&item.recent_maintenance_penalty.to_le_bytes());

        match TestPma::decode_maintenance_queue(&bytes) {
            Err(GraphStoreError::Hydration(HydrationError::InvalidMaintenanceQueueHeader(
                kind,
            ))) => {
                assert_eq!(kind, RegionKind::MaintenanceQueue)
            }
            other => panic!("expected invalid maintenance-queue header, got {other:?}"),
        }
    }

    #[test]
    fn facade_rejects_unsupported_maintenance_queue_format_version() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&TestPma::SERIALIZED_MAINTENANCE_QUEUE_MAGIC);
        bytes.extend_from_slice(&99u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());

        match TestPma::decode_maintenance_queue(&bytes) {
            Err(GraphStoreError::Hydration(HydrationError::UnsupportedFormatVersion {
                kind,
                expected,
                actual,
            })) => {
                assert_eq!(kind, RegionKind::MaintenanceQueue);
                assert_eq!(expected, TestPma::SERIALIZED_MAINTENANCE_QUEUE_VERSION);
                assert_eq!(actual, 99);
            }
            other => panic!("expected unsupported queue format version error, got {other:?}"),
        }
    }

    #[test]
    fn facade_rejects_corrupted_maintenance_queue_checksum() {
        let item = GraphMaintenanceWorkItem {
            vertex_ref: NodeId::from(89u8).into(),
            anchor_ordinal: 3,
            start_ordinal: 2,
            end_ordinal_exclusive: 5,
            priority_score: 42,
            last_maintenance_epoch: Some(9),
            recent_maintenance_penalty: 7,
        };
        let mut bytes = TestPma::encode_maintenance_queue(&[item]).expect("encode queue");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;

        match TestPma::decode_maintenance_queue(&bytes) {
            Err(GraphStoreError::Hydration(HydrationError::ChecksumMismatch {
                kind,
                expected,
                actual,
            })) => {
                assert_eq!(kind, RegionKind::MaintenanceQueue);
                assert_ne!(expected, actual);
            }
            other => panic!("expected maintenance queue checksum mismatch, got {other:?}"),
        }
    }

    #[test]
    fn facade_rejects_premgq1_maintenance_queue_storage_projection() {
        let memory = VecMemory::default();
        let facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let item = GraphMaintenanceWorkItem {
            vertex_ref: NodeId::from(90u8).into(),
            anchor_ordinal: 3,
            start_ordinal: 2,
            end_ordinal_exclusive: 5,
            priority_score: 42,
            last_maintenance_epoch: Some(9),
            recent_maintenance_penalty: 7,
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&u64::from(item.vertex_ref).to_le_bytes());
        bytes.extend_from_slice(&(item.anchor_ordinal as u64).to_le_bytes());
        bytes.extend_from_slice(&(item.start_ordinal as u64).to_le_bytes());
        bytes.extend_from_slice(&(item.end_ordinal_exclusive as u64).to_le_bytes());
        bytes.extend_from_slice(&item.priority_score.to_le_bytes());
        bytes.extend_from_slice(&item.last_maintenance_epoch.unwrap().to_le_bytes());
        bytes.extend_from_slice(&item.recent_maintenance_penalty.to_le_bytes());
        TestPma::write_maintenance_queue_slot_bytes(&facade.property_slots, &bytes)
            .expect("write invalid queue bytes to fixed slot");

        match facade.try_read_maintenance_queue_storage_projection_from_stable_memory(
            facade.memory.as_ref(),
        ) {
            Err(GraphStoreError::Hydration(HydrationError::InvalidMaintenanceQueueHeader(
                kind,
            ))) => {
                assert_eq!(kind, RegionKind::MaintenanceQueue)
            }
            other => panic!("expected invalid maintenance-queue header, got {other:?}"),
        }
    }

    #[test]
    fn facade_can_read_vertex_entry_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let forward = facade
            .try_read_forward_vertex_entry_from_stable_memory(&memory, 0)
            .expect("forward direct read should succeed");
        let reverse = facade
            .try_read_reverse_vertex_entry_from_stable_memory(&memory, 0)
            .expect("reverse direct read should succeed");

        assert_eq!(
            forward,
            Some(VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET))
        );
        assert_eq!(
            reverse,
            Some(VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET))
        );
    }

    #[test]
    fn facade_can_read_reserved_span_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let forward = facade
            .try_read_forward_vertex_reserved_span_from_stable_memory(&memory, 0)
            .expect("forward reserved span read should succeed");
        let reverse = facade
            .try_read_reverse_vertex_reserved_span_from_stable_memory(&memory, 0)
            .expect("reverse reserved span read should succeed");

        assert_eq!(forward, Some(1));
        assert_eq!(reverse, Some(1));
    }

    #[test]
    fn facade_can_read_base_entries_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let forward = facade
            .try_read_forward_vertex_base_entries_from_stable_memory(&memory, 0)
            .expect("forward base-entry read should succeed");
        let reverse = facade
            .try_read_reverse_vertex_base_entries_from_stable_memory(&memory, 0)
            .expect("reverse base-entry read should succeed");

        assert_eq!(
            forward,
            Some(vec![EdgeEntry::new(
                NodeId::from(2u8),
                EdgeMeta::new(7, false)
            )])
        );
        assert_eq!(
            reverse,
            Some(vec![EdgeEntry::new(
                NodeId::from(1u8),
                EdgeMeta::new(7, false)
            )])
        );
    }

    #[test]
    fn facade_can_read_edge_entries_by_ref_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let forward = facade
            .try_read_forward_edge_entries_by_ref_from_stable_memory(
                &memory,
                crate::low_level::EdgeRef::new(0, 0),
                1,
            )
            .expect("forward edge-ref read should succeed");
        let reverse = facade
            .try_read_reverse_edge_entries_by_ref_from_stable_memory(
                &memory,
                crate::low_level::EdgeRef::new(0, 0),
                1,
            )
            .expect("reverse edge-ref read should succeed");

        assert_eq!(
            forward,
            vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))]
        );
        assert_eq!(
            reverse,
            vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))]
        );
    }

    #[test]
    fn facade_can_read_base_edge_ref_and_entry_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let forward_ref = facade
            .try_read_forward_vertex_base_edge_ref_from_stable_memory(&memory, 0, 0)
            .expect("forward base-edge-ref read should succeed");
        let reverse_ref = facade
            .try_read_reverse_vertex_base_edge_ref_from_stable_memory(&memory, 0, 0)
            .expect("reverse base-edge-ref read should succeed");
        let forward_entry = facade
            .try_read_forward_vertex_base_entry_from_stable_memory(&memory, 0, 0)
            .expect("forward base-entry read should succeed");
        let reverse_entry = facade
            .try_read_reverse_vertex_base_entry_from_stable_memory(&memory, 0, 0)
            .expect("reverse base-entry read should succeed");

        assert_eq!(forward_ref, Some(crate::low_level::EdgeRef::new(0, 0)));
        assert_eq!(reverse_ref, Some(crate::low_level::EdgeRef::new(0, 0)));
        assert_eq!(
            forward_entry,
            Some(EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)))
        );
        assert_eq!(
            reverse_entry,
            Some(EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)))
        );
    }

    #[test]
    fn facade_prefers_runtime_vertex_entry_when_vertex_table_is_dirty() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let appended = facade
            .append_empty_vertex_pair()
            .expect("append should mark vertex table dirty");

        let forward =
            facade.read_forward_vertex_entry_preferring_stable_memory(&memory, appended.0);
        let reverse =
            facade.read_reverse_vertex_entry_preferring_stable_memory(&memory, appended.1);

        assert_eq!(
            forward,
            Some(VertexEntry::new(EdgeIndex::new(1), 0, EMPTY_LOG_OFFSET))
        );
        assert_eq!(
            reverse,
            Some(VertexEntry::new(EdgeIndex::new(1), 0, EMPTY_LOG_OFFSET))
        );
    }

    #[test]
    fn facade_can_read_vertex_entry_ranges_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade.append_empty_vertex_pair_and_write(&memory).unwrap();

        let forward = facade
            .try_read_forward_vertex_entries_from_stable_memory(&memory, 0, 2)
            .expect("forward range read should succeed");
        let reverse = facade
            .try_read_reverse_vertex_entries_from_stable_memory(&memory, 0, 2)
            .expect("reverse range read should succeed");

        assert_eq!(
            forward,
            vec![
                VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(1), 0, EMPTY_LOG_OFFSET),
            ]
        );
        assert_eq!(
            reverse,
            vec![
                VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(1), 0, EMPTY_LOG_OFFSET),
            ]
        );
    }

    #[test]
    fn facade_prefers_runtime_vertex_entry_ranges_when_vertex_table_is_dirty() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade.append_empty_vertex_pair().unwrap();

        let forward = facade.read_forward_vertex_entries_preferring_stable_memory(&memory, 0, 2);
        let reverse = facade.read_reverse_vertex_entries_preferring_stable_memory(&memory, 0, 2);

        assert_eq!(
            forward,
            vec![
                VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(1), 0, EMPTY_LOG_OFFSET),
            ]
        );
        assert_eq!(
            reverse,
            vec![
                VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(1), 0, EMPTY_LOG_OFFSET),
            ]
        );
    }

    #[test]
    fn facade_can_summarize_vertex_window_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade.append_empty_vertex_pair_and_write(&memory).unwrap();

        let forward = facade
            .try_summarize_forward_vertex_window_from_stable_memory(&memory, 0, 2)
            .expect("forward summary should succeed")
            .expect("forward window should exist");
        let reverse = facade
            .try_summarize_reverse_vertex_window_from_stable_memory(&memory, 0, 2)
            .expect("reverse summary should succeed")
            .expect("reverse window should exist");

        assert_eq!(forward.base_start, EdgeIndex::new(0));
        assert_eq!(forward.live_end_exclusive, EdgeIndex::new(1));
        assert_eq!(forward.total_live_degree, 1);
        assert_eq!(forward.max_live_degree, 1);
        assert_eq!(forward.vertices_with_overflow, 0);
        assert_eq!(reverse.base_start, EdgeIndex::new(0));
        assert_eq!(reverse.live_end_exclusive, EdgeIndex::new(1));
        assert_eq!(reverse.total_live_degree, 1);
        assert_eq!(reverse.max_live_degree, 1);
        assert_eq!(reverse.vertices_with_overflow, 0);
    }

    #[test]
    fn facade_prefers_runtime_vertex_window_summary_when_vertex_table_is_dirty() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade.append_empty_vertex_pair().unwrap();

        let summary = facade
            .summarize_forward_vertex_window_preferring_stable_memory(&memory, 0, 2)
            .expect("window summary should exist");

        assert_eq!(summary.base_start, EdgeIndex::new(0));
        assert_eq!(summary.live_end_exclusive, EdgeIndex::new(1));
        assert_eq!(summary.total_live_degree, 1);
        assert_eq!(summary.max_live_degree, 1);
        assert_eq!(summary.vertices_with_overflow, 0);
    }

    #[test]
    fn facade_can_estimate_vertex_window_reserve_hint_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade.append_empty_vertex_pair_and_write(&memory).unwrap();

        let hint = facade
            .try_estimate_forward_vertex_window_reserve_hint_from_stable_memory(&memory, 0, 2, 5, 1)
            .expect("reserve hint should succeed")
            .expect("reserve hint should exist");

        assert_eq!(hint.live_span_len_lower_bound, 1);
        assert_eq!(hint.target_base_len_lower_bound, 2);
        assert_eq!(hint.extra_slots_for_anchor_degree, 2);
        assert_eq!(hint.preferred_reserved_base_len_lower_bound, 4);
        assert_eq!(hint.total_weight, 3);
        assert_eq!(hint.vertices_with_overflow, 0);
    }

    #[test]
    fn facade_prefers_runtime_vertex_window_reserve_hint_when_vertex_table_is_dirty() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade.append_empty_vertex_pair().unwrap();

        let hint = facade
            .estimate_forward_vertex_window_reserve_hint_preferring_stable_memory(
                &memory, 0, 2, 5, 1,
            )
            .expect("reserve hint should exist");

        assert_eq!(hint.live_span_len_lower_bound, 1);
        assert_eq!(hint.target_base_len_lower_bound, 2);
        assert_eq!(hint.extra_slots_for_anchor_degree, 2);
        assert_eq!(hint.preferred_reserved_base_len_lower_bound, 4);
        assert_eq!(hint.total_weight, 3);
        assert_eq!(hint.vertices_with_overflow, 0);
    }

    #[test]
    fn facade_refresh_and_write_dirty_round_trips() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let src = NodeId::from(1u8);
        let dst = NodeId::from(3u8);
        facade
            .graph
            .forward
            .replace_base_entry(0, 0, EdgeEntry::new(dst, EdgeMeta::new(9, false)))
            .unwrap();
        facade
            .graph
            .reverse
            .replace_base_entry(0, 0, EdgeEntry::new(src, EdgeMeta::new(9, false)))
            .unwrap();
        let mem_rc = Rc::clone(&facade.memory);
        let _ = facade
            .refresh_and_write_dirty_to_stable_memory(mem_rc.as_ref())
            .unwrap();

        let rehydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .unwrap();
        assert_eq!(
            rehydrated.graph.forward.0.base_entries.get(0).copied(),
            Some(EdgeEntry::new(dst, EdgeMeta::new(9, false)))
        );
        assert_eq!(
            rehydrated.graph.reverse.0.base_entries.get(0).copied(),
            Some(EdgeEntry::new(src, EdgeMeta::new(9, false)))
        );
    }

    #[test]
    fn facade_property_stores_round_trip_through_stable_memory() {
        let memory = VecMemory::default();
        let mut facade =
            GraphStore::bootstrap_empty_with_bucket_size(BucketSizeInPages::new(1), memory.clone())
                .expect("bootstrap");
        let node_id = NodeId::from(11u8);

        facade
            .set_node_property_value(
                node_id,
                "profile",
                &Value::Text("x".repeat((crate::low_level::WASM_PAGE_SIZE as usize) + 512)),
            )
            .expect("set node property");
        facade
            .set_edge_property_value(77, "weight", &Value::Int64(9))
            .expect("set edge property");
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(mem_rc.as_ref())
            .expect("write all");

        let rehydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .unwrap();
        assert_eq!(
            rehydrated.get_node_property_value(node_id, "profile"),
            Some(Value::Text(
                "x".repeat((crate::low_level::WASM_PAGE_SIZE as usize) + 512)
            ))
        );
        assert_eq!(
            rehydrated.get_edge_property_value(77, "weight"),
            Some(Value::Int64(9))
        );
    }

    #[test]
    fn facade_property_store_direct_write_round_trips_without_dirty_flags() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(21u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("u21".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(901, "weight", &Value::Int64(42))
            .expect("set edge property");
        assert!(!facade.node_property_store_dirty);
        assert!(!facade.edge_property_store_dirty);

        let mem_rc = Rc::clone(&facade.memory);
        let refreshed = facade
            .refresh_and_write_dirty_to_stable_memory(mem_rc.as_ref())
            .expect("write dirty");
        assert_eq!(refreshed, (Vec::new(), Vec::new()));
        assert!(!facade.node_property_store_dirty);
        assert!(!facade.edge_property_store_dirty);

        let rehydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .unwrap();
        assert_eq!(
            rehydrated.scan_node_properties(node_id).get("uid"),
            Some(&Value::Text("u21".into()))
        );
        assert_eq!(
            rehydrated.scan_edge_properties(901).get("weight"),
            Some(&Value::Int64(42))
        );
    }

    #[test]
    fn facade_hydrate_query_mutate_flush_rehydrate_roundtrip_contract() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(41u8);
        let edge_id = 941u64;

        // mutate
        facade
            .set_node_property_value(node_id, "uid", &Value::Text("u41".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(edge_id, "weight", &Value::Int64(41))
            .expect("set edge property");
        let mem_rc_flush = Rc::clone(&facade.memory);
        facade
            .refresh_and_write_dirty_to_stable_memory(mem_rc_flush.as_ref())
            .expect("flush dirty");

        // rehydrate + query
        let mut rehydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .expect("rehydrate");
        assert_eq!(
            rehydrated.scan_node_ids_by_property_eq("uid", &Value::Text("u41".into())),
            vec![node_id]
        );
        assert_eq!(
            rehydrated.scan_edge_ids_by_property_eq("weight", &Value::Int64(41)),
            vec![edge_id]
        );
        assert!(!rehydrated.node_property_store_dirty);
        assert!(!rehydrated.edge_property_store_dirty);

        // mutate again + flush + rehydrate again
        rehydrated
            .set_node_property_value(node_id, "uid", &Value::Text("u41b".into()))
            .expect("overwrite node property");
        rehydrated
            .remove_edge_property_value(edge_id, "weight")
            .expect("remove edge property");
        let mem_rc2 = Rc::clone(&rehydrated.memory);
        rehydrated
            .refresh_and_write_dirty_to_stable_memory(mem_rc2.as_ref())
            .expect("flush dirty second time");

        let rehydrated2 = GraphStore::hydrate_from_stable_memory(
            rehydrated.manager.borrow().clone(),
            (*rehydrated.memory).clone(),
        )
        .expect("rehydrate second");
        assert!(
            rehydrated2
                .scan_node_ids_by_property_eq("uid", &Value::Text("u41".into()))
                .is_empty()
        );
        assert_eq!(
            rehydrated2.scan_node_ids_by_property_eq("uid", &Value::Text("u41b".into())),
            vec![node_id]
        );
        assert!(
            rehydrated2
                .scan_edge_ids_by_property_eq("weight", &Value::Int64(41))
                .is_empty()
        );
    }

    #[test]
    fn facade_property_index_tracks_equality_updates_and_removals() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(31u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set initial property");
        assert_eq!(
            facade.scan_node_ids_by_property_eq("uid", &Value::Text("alice".into())),
            vec![node_id]
        );

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("bob".into()))
            .expect("overwrite property");
        assert!(
            facade
                .scan_node_ids_by_property_eq("uid", &Value::Text("alice".into()))
                .is_empty()
        );
        assert_eq!(
            facade.scan_node_ids_by_property_eq("uid", &Value::Text("bob".into())),
            vec![node_id]
        );

        facade
            .remove_node_property_value(node_id, "uid")
            .expect("remove property");
        assert!(
            facade
                .scan_node_ids_by_property_eq("uid", &Value::Text("bob".into()))
                .is_empty()
        );
    }

    #[test]
    fn facade_property_index_sync_failure_rolls_back_property_store() {
        // Thread-local injection flags so parallel tests do not consume each other's hooks.
        let memory = VecMemory::default();

        //
        // Node property: injected index-bind error
        //
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(1u8);
        facade
            .set_node_property_value(node_id, "a", &Value::Text("hello".into()))
            .expect("seed node property");
        super::FAIL_NEXT_NODE_PROPERTY_INDEX_SYNC_TEST.with(|c| c.set(true));
        let err = facade
            .set_node_property_value(node_id, "b", &Value::Text("world".into()))
            .expect_err("injected node sync failure");
        assert!(
            matches!(
                err,
                PropertyStoreError::PropertyIndex(
                    PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage
                )
            ),
            "unexpected err: {err:?}"
        );
        assert_eq!(
            facade.scan_node_properties(node_id).get("a"),
            Some(&Value::Text("hello".into()))
        );
        assert!(!facade.scan_node_properties(node_id).contains_key("b"));

        //
        // Edge property: injected `try_sync` error
        //
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let edge_id = 77u64;
        facade
            .set_edge_property_value(edge_id, "kind", &Value::Text("follows".into()))
            .expect("seed edge property");
        super::FAIL_NEXT_EDGE_PROPERTY_INDEX_SYNC_TEST.with(|c| c.set(true));
        let err = facade
            .set_edge_property_value(edge_id, "role", &Value::Text("actor".into()))
            .expect_err("injected edge sync failure");
        assert!(
            matches!(
                err,
                PropertyStoreError::PropertyIndex(
                    PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage
                )
            ),
            "unexpected err: {err:?}"
        );
        assert_eq!(
            facade.scan_edge_properties(edge_id).get("kind"),
            Some(&Value::Text("follows".into()))
        );
        assert!(!facade.scan_edge_properties(edge_id).contains_key("role"));
    }

    #[test]
    fn facade_property_index_mutation_summary_reports_touched_nodes() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(32u8);

        let first = facade
            .set_node_property_value_with_summary(node_id, "uid", &Value::Text("alice".into()))
            .expect("set initial property with summary");
        assert_eq!(
            first.sections,
            GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert_eq!(
            first.node_store_operations,
            vec![PropertyIndexNodeStoreMutationKind::LocalUpdate]
        );
        assert!(first.fallback_reasons.is_empty());
        assert!(!first.touched_node_ids.is_empty());
        assert!(first.allocated_node_ids.is_empty());
        assert!(first.freed_node_ids.is_empty());

        let overwrite = facade
            .set_node_property_value_with_summary(node_id, "uid", &Value::Text("bob".into()))
            .expect("overwrite property with summary");
        assert_eq!(
            overwrite.sections,
            GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert_eq!(
            overwrite.node_store_operations,
            vec![
                PropertyIndexNodeStoreMutationKind::Collapse,
                PropertyIndexNodeStoreMutationKind::LocalUpdate,
            ]
        );
        assert!(overwrite.fallback_reasons.is_empty());
        assert!(!overwrite.touched_node_ids.is_empty());

        let idempotent = facade
            .set_node_property_value_with_summary(node_id, "uid", &Value::Text("bob".into()))
            .expect("idempotent set same encoded value");
        assert_eq!(
            idempotent.sections,
            GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: false,
            }
        );
        assert!(idempotent.node_store_operations.is_empty());
        assert!(idempotent.touched_node_ids.is_empty());

        let removal = facade
            .remove_node_property_value_with_summary(node_id, "uid")
            .expect("remove property with summary");
        assert_eq!(
            removal.sections,
            GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert_eq!(
            removal.node_store_operations,
            vec![PropertyIndexNodeStoreMutationKind::Collapse]
        );
        assert!(removal.fallback_reasons.is_empty());
        assert!(!removal.touched_node_ids.is_empty());
        assert!(removal.freed_node_ids.is_empty());
        assert!(removal.allocated_node_ids.is_empty());
        let metrics = facade.production_metrics_snapshot();
        assert_eq!(metrics.property_index_fallback_total, 0);
    }

    #[test]
    fn facade_property_index_mutation_summary_reports_edge_updates() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");

        let set = facade
            .set_edge_property_value_with_summary(701, "weight", &Value::Int64(5))
            .expect("set edge property with summary");
        assert_eq!(
            set.sections,
            GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert!(!set.touched_node_ids.is_empty());

        let remove = facade
            .remove_edge_property_value_with_summary(701, "weight")
            .expect("remove edge property with summary");
        assert_eq!(
            remove.sections,
            GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert!(!remove.touched_node_ids.is_empty());
    }

    #[test]
    fn facade_property_mutation_write_summary_flushes_and_round_trips() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(33u8);

        let mem_rc = Rc::clone(&facade.memory);
        let summary = facade
            .set_node_property_value_and_write(
                node_id,
                "uid",
                &Value::Text("carol".into()),
                mem_rc.as_ref(),
            )
            .expect("set property and write");
        assert_eq!(
            summary.flushed_sections,
            GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert_eq!(summary.flushed_sections, summary.mutation.sections);
        assert!(!facade.node_property_store_is_dirty());
        assert!(
            facade
                .scan_node_ids_by_property_eq_preferring_stable_memory(
                    mem_rc.as_ref(),
                    "uid",
                    &Value::Text("carol".into()),
                )
                .contains(&node_id)
        );

        let rehydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .unwrap();
        assert_eq!(
            rehydrated.scan_node_ids_by_property_eq("uid", &Value::Text("carol".into())),
            vec![node_id]
        );
        assert!(matches!(
            facade.last_write_event(),
            Some(GraphStoreFacadeWriteEvent::Property(_))
        ));
        let event_projection = facade
            .last_write_event()
            .and_then(GraphStoreFacadeWriteEvent::property_projection)
            .expect("property event projection");
        assert_eq!(summary.projection(), event_projection);
        assert!(matches!(
            facade.write_history(),
            [GraphStoreFacadeWriteEvent::Property(_)]
        ));
        assert_eq!(
            project_facade_write_event(facade.write_history().last().expect("last facade event")),
            vec![GraphStoreWriteEventProjection::Property(
                summary.projection()
            )]
        );
    }

    #[test]
    fn facade_edge_property_mutation_write_summary_flushes_and_clears_dirty_state() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");

        let mem_rc = Rc::clone(&facade.memory);
        let set = facade
            .set_edge_property_value_and_write(702, "weight", &Value::Int64(9), mem_rc.as_ref())
            .expect("set edge property and write");
        assert_eq!(
            set.flushed_sections,
            GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert!(!facade.edge_property_store_is_dirty());

        let remove = facade
            .remove_edge_property_value_and_write(702, "weight", mem_rc.as_ref())
            .expect("remove edge property and write");
        assert_eq!(
            remove.flushed_sections,
            GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert!(!facade.edge_property_store_is_dirty());
        assert!(
            facade
                .scan_edge_ids_by_property_eq_preferring_stable_memory(
                    mem_rc.as_ref(),
                    "weight",
                    &Value::Int64(9),
                )
                .is_empty()
        );
        let metrics = facade.production_metrics_snapshot();
        assert!(metrics.edge_eq_scan_count >= 1);
        assert!(metrics.edge_eq_scan_total_nanos > 0);
        assert!(matches!(
            facade.write_history(),
            [
                GraphStoreFacadeWriteEvent::Property(_),
                GraphStoreFacadeWriteEvent::Property(_)
            ]
        ));
        assert_projected_history(
            facade.write_history(),
            vec![
                GraphStoreWriteEventProjection::Property(set.projection()),
                GraphStoreWriteEventProjection::Property(remove.projection()),
            ],
        );
        assert_eq!(
            project_facade_write_event(facade.write_history().last().expect("last facade event")),
            vec![GraphStoreWriteEventProjection::Property(
                remove.projection()
            )]
        );
    }

    #[test]
    fn facade_records_edge_write_events_in_unified_history() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(81u8);
        let dst = NodeId::from(82u8);
        let bootstrap = facade
            .bootstrap_vertex_refs_and_edges_and_write(
                &[src.into(), dst.into()],
                &[(9001, 0, 1, 7)],
                &memory,
            )
            .expect("bootstrap graph");
        let src_ordinal = bootstrap.vertex_ordinals[0].forward_ordinal;
        let dst_ordinal = bootstrap.vertex_ordinals[1].reverse_ordinal;

        let replace = facade
            .replace_edge_pair_and_write(
                EdgeReplaceSpec {
                    edge_id: 9001,
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: src.into(),
                        src_ordinal,
                        dst_vertex_ref: dst.into(),
                        dst_ordinal,
                    },
                    locators: EdgePairLogicalLocators {
                        forward: crate::low_level::LogicalEdgeLocator::base(
                            crate::low_level::SurfaceKind::Forward,
                            src,
                            0,
                        ),
                        reverse: crate::low_level::LogicalEdgeLocator::base(
                            crate::low_level::SurfaceKind::Reverse,
                            dst,
                            0,
                        ),
                    },
                    edge_meta: 9u16.into(),
                },
                &memory,
            )
            .expect("replace edge");
        assert_eq!(replace.mutation.0, GraphMutationPath::Base);

        let delete = facade
            .tombstone_edge_pair_and_write(
                EdgeTombstoneSpec {
                    edge_id: 9001,
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: src.into(),
                        src_ordinal,
                        dst_vertex_ref: dst.into(),
                        dst_ordinal,
                    },
                    locators: EdgePairLogicalLocators {
                        forward: crate::low_level::LogicalEdgeLocator::base(
                            crate::low_level::SurfaceKind::Forward,
                            src,
                            0,
                        ),
                        reverse: crate::low_level::LogicalEdgeLocator::base(
                            crate::low_level::SurfaceKind::Reverse,
                            dst,
                            0,
                        ),
                    },
                },
                &memory,
            )
            .expect("delete edge");
        assert_eq!(delete.mutation, GraphMutationPath::Base);
        assert!(matches!(
            facade.write_history(),
            [
                GraphStoreFacadeWriteEvent::BootstrapGraph(_),
                GraphStoreFacadeWriteEvent::ReplaceEdge(_),
                GraphStoreFacadeWriteEvent::DeleteEdge(_)
            ]
        ));
        assert!(matches!(
            facade.last_write_event(),
            Some(GraphStoreFacadeWriteEvent::DeleteEdge(_))
        ));
    }

    #[test]
    fn facade_property_index_snapshot_round_trips_through_stable_memory() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(41u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(777, "weight", &Value::Int64(5))
            .expect("set edge property");
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(mem_rc.as_ref())
            .expect("write all");

        let rehydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .unwrap();
        assert_eq!(
            rehydrated.scan_node_ids_by_property_eq("uid", &Value::Text("alice".into())),
            vec![node_id]
        );
        assert_eq!(
            rehydrated.scan_edge_ids_by_property_eq("weight", &Value::Int64(5)),
            vec![777]
        );
    }

    #[test]
    fn facade_writes_fixed_slot_property_equality_map_with_nonempty_btree() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(42u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(778, "weight", &Value::Int64(5))
            .expect("set edge property");
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(mem_rc.as_ref())
            .expect("write all");

        let slots = facade.fixed_memory_slots();
        let equality_map = open_fixed_slot_property_equality_map(&slots);
        let snapshot = snapshot_fixed_slot_property_equality_map(&equality_map);
        assert!(snapshot.len() >= 2);

        let rehydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .unwrap();
        assert_eq!(
            rehydrated.scan_node_ids_by_property_eq("uid", &Value::Text("alice".into())),
            vec![node_id]
        );
        assert_eq!(
            rehydrated.scan_edge_ids_by_property_eq("weight", &Value::Int64(5)),
            vec![778]
        );
    }

    #[test]
    fn facade_records_ensure_capacity_and_insert_in_shared_history() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(91u8);
        let dst = NodeId::from(92u8);

        let bootstrap = facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        let src_mapping = &bootstrap.vertex_ordinals[0];
        let dst_mapping = &bootstrap.vertex_ordinals[1];

        let ensure = facade
            .ensure_local_capacity_for_incoming_live_entries_and_write(
                RebalancePrepareSpec {
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: src.into(),
                        src_ordinal: src_mapping.forward_ordinal,
                        dst_vertex_ref: dst.into(),
                        dst_ordinal: dst_mapping.reverse_ordinal,
                    },
                    planned_incoming_live_entries: 1,
                    forward_rebalance_vertex_ids: &[src.into(), dst.into()],
                    forward_rebalance_base_edge_ids_by_ordinal: &[Vec::new(), Vec::new()],
                },
                &memory,
            )
            .expect("ensure capacity");
        let insert = facade
            .insert_edge_pair_with_local_rebalance_and_write(
                RebalanceInsertSpec {
                    edge_id: 9901,
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: src.into(),
                        src_ordinal: src_mapping.forward_ordinal,
                        dst_vertex_ref: dst.into(),
                        dst_ordinal: dst_mapping.reverse_ordinal,
                    },
                    edge_meta: 7u16.into(),
                    planned_incoming_live_entries: 1,
                    forward_rebalance_vertex_ids: &[src.into(), dst.into()],
                    forward_rebalance_base_edge_ids_by_ordinal: &[vec![9901], Vec::new()],
                },
                &memory,
            )
            .expect("insert edge");

        assert_projected_history(
            facade.write_history(),
            vec![
                GraphStoreWriteEventProjection::BootstrapGraph(bootstrap.projection()),
                GraphStoreWriteEventProjection::EnsureCapacity(
                    GraphStoreEnsureCapacityProjection::from_summary(&ensure),
                ),
                GraphStoreWriteEventProjection::InsertEdge(
                    GraphStoreInsertEdgeProjection::from_summary(&insert),
                ),
            ],
        );
    }

    #[test]
    fn facade_can_collect_maintenance_candidates() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(31u8);
        let dst = NodeId::from(32u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(32u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(33u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            900,
            EdgeEntry::new(
                NodeId::from(34u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(31u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(35u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            901,
            EdgeEntry::new(
                NodeId::from(31u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];

        let candidates = facade
            .collect_maintenance_candidates(&[src.into(), dst.into()])
            .expect("maintenance candidates");

        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].vertex_ref, src.into());
        assert!(candidates[0].has_overflow_backlog());
    }

    #[test]
    fn facade_can_collect_epoch_aware_maintenance_candidates() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(36u8);
        let dst = NodeId::from(37u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        });
        facade.set_maintenance_fairness(3, 100_000);
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(37u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(38u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            900,
            EdgeEntry::new(
                NodeId::from(39u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(36u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(40u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            901,
            EdgeEntry::new(
                NodeId::from(36u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.record_recent_maintenance_window(0, 1, 10);

        let candidates = facade
            .collect_maintenance_candidates_at_epoch(&[src.into(), dst.into()], 11)
            .expect("maintenance candidates");

        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].vertex_ref, dst.into());
        assert_eq!(candidates[0].recent_maintenance_penalty, 0);
        assert_eq!(candidates[1].vertex_ref, src.into());
        assert_eq!(candidates[1].last_maintenance_epoch, Some(10));
        assert!(candidates[1].recent_maintenance_penalty > 0);
    }

    #[test]
    fn facade_can_collect_and_run_maintenance_work_item() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(38u8);
        let dst = NodeId::from(39u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(39u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(40u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            995,
            EdgeEntry::new(
                NodeId::from(41u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(38u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(42u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            996,
            EdgeEntry::new(
                NodeId::from(38u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];

        let work_item = facade
            .collect_maintenance_work_items(&[src.into(), dst.into()])
            .expect("work items")
            .into_iter()
            .next()
            .expect("one work item");
        let plan = facade
            .plan_maintenance_cycle_from_work_item(work_item)
            .expect("plan from work item");
        assert_eq!(plan.candidate.vertex_ref, src.into());

        let summary = facade
            .run_one_maintenance_cycle_from_work_item_with_segment_replacement_and_write(
                work_item,
                &[src.into(), dst.into()],
                &[vec![710, 711], vec![712]],
                &memory,
                161,
            )
            .expect("maintenance cycle write")
            .expect("maintenance summary");

        assert_eq!(summary.candidate.vertex_ref, src.into());
        assert_eq!(
            summary
                .rebalance
                .apply
                .segments
                .forward
                .new_segment
                .segment_id,
            1
        );
    }

    #[test]
    fn facade_can_format_maintenance_queue_snapshot() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(58u8);
        let dst = NodeId::from(59u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            maintenance_recent_epoch_window: 3,
            maintenance_recent_epoch_penalty: 100_000,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(59u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(60u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(58u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(61u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1003,
            EdgeEntry::new(
                NodeId::from(62u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1004,
            EdgeEntry::new(
                NodeId::from(58u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.record_recent_maintenance_window(0, 1, 10);

        assert_eq!(
            facade.rebuild_maintenance_queue_at_epoch(&[src.into(), dst.into()], 11),
            Some(2)
        );

        let queue = facade.maintenance_queue_projection();
        assert_eq!(queue.len(), 2);
        let src_item = queue
            .iter()
            .find(|item| item.vertex_ref == src.into())
            .expect("src maintenance queue item");
        assert_eq!(src_item.window_start_ordinal, 0);
        assert_eq!(src_item.window_end_ordinal_exclusive, 1);
        assert!(src_item.recent_maintenance_penalty > 0);

        let formatted = facade.formatted_maintenance_queue();
        assert_eq!(formatted.len(), 2);
        assert!(
            formatted
                .iter()
                .any(|line| line.contains("maintenance-queue vertex=58")
                    && line.contains("window=(0, 1)"))
        );
        let formatted_storage = facade.formatted_maintenance_queue_storage();
        assert!(formatted_storage.contains("maintenance-queue-storage"));
        assert!(formatted_storage.contains("queue=2"));
    }

    #[test]
    fn facade_can_rebuild_and_run_next_queued_maintenance_cycle() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(43u8);
        let dst = NodeId::from(44u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(44u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(45u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            997,
            EdgeEntry::new(
                NodeId::from(46u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(43u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(47u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            998,
            EdgeEntry::new(
                NodeId::from(43u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];

        assert_eq!(
            facade.rebuild_maintenance_queue(&[src.into(), dst.into()]),
            Some(1)
        );
        assert_eq!(facade.maintenance_queue().len(), 1);

        let summary = facade
            .run_next_queued_maintenance_cycle_with_segment_replacement_and_write(
                &[src.into(), dst.into()],
                &[vec![720, 721], vec![722]],
                &memory,
                181,
            )
            .expect("queued maintenance write")
            .expect("queued maintenance summary");

        assert_eq!(summary.candidate.vertex_ref, src.into());
        assert_eq!(facade.maintenance_queue().len(), 0);
    }

    #[test]
    fn facade_can_refresh_retained_maintenance_queue() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(48u8);
        let dst = NodeId::from(49u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            maintenance_recent_epoch_window: 3,
            maintenance_recent_epoch_penalty: 100_000,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(49u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(50u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(48u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(51u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            999,
            EdgeEntry::new(
                NodeId::from(52u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1000,
            EdgeEntry::new(
                NodeId::from(48u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];

        assert_eq!(
            facade.rebuild_maintenance_queue(&[src.into(), dst.into()]),
            Some(2)
        );
        facade.graph.record_recent_maintenance_window(0, 1, 10);

        let refreshed = facade
            .refresh_maintenance_queue_at_epoch(&[src.into(), dst.into()], 11)
            .expect("refresh queue");

        assert_eq!(refreshed, 2);
        assert_eq!(facade.maintenance_queue()[0].vertex_ref, dst.into());
        assert_eq!(facade.maintenance_queue()[1].vertex_ref, src.into());
        assert!(facade.maintenance_queue()[1].recent_maintenance_penalty > 0);
        match facade.last_write_event() {
            Some(GraphStoreFacadeWriteEvent::MaintenanceQueue(event)) => {
                assert_eq!(event.action, GraphStoreMaintenanceQueueAction::Refresh);
                assert_eq!(event.queue_len_before, 2);
                assert_eq!(event.queue_len_after, 2);
                assert_eq!(
                    event.format_version,
                    TestPma::SERIALIZED_MAINTENANCE_QUEUE_VERSION
                );
                assert!(event.persisted_bytes > 0);
            }
            other => panic!("expected maintenance-queue write event, got {other:?}"),
        }
    }

    #[test]
    fn facade_can_run_queued_maintenance_batch_with_refresh() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(53u8);
        let dst = NodeId::from(54u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(54u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(55u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1001,
            EdgeEntry::new(
                NodeId::from(56u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(53u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(57u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1002,
            EdgeEntry::new(
                NodeId::from(53u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];

        assert_eq!(
            facade.rebuild_maintenance_queue(&[src.into(), dst.into()]),
            Some(1)
        );

        let summary = facade
            .run_queued_maintenance_cycles_with_segment_replacement_and_write(
                &[src.into(), dst.into()],
                &[vec![730, 731], vec![732]],
                &memory,
                220,
                2,
                0,
            )
            .expect("queued maintenance batch");

        assert_eq!(summary.cycles.len(), 1);
        assert_eq!(summary.queue_len_before, 1);
        assert_eq!(summary.queue_len_after, 0);
        assert!(facade.maintenance_queue().is_empty());
    }

    #[test]
    fn facade_records_maintenance_queue_rebuild_event() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(63u8);
        let dst = NodeId::from(64u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(64u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(65u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1005,
            EdgeEntry::new(
                NodeId::from(66u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(63u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(67u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1006,
            EdgeEntry::new(
                NodeId::from(63u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];

        assert_eq!(
            facade.rebuild_maintenance_queue(&[src.into(), dst.into()]),
            Some(2)
        );
        match facade.last_write_event() {
            Some(GraphStoreFacadeWriteEvent::MaintenanceQueue(event)) => {
                assert_eq!(event.action, GraphStoreMaintenanceQueueAction::Rebuild);
                assert_eq!(event.queue_len_before, 0);
                assert_eq!(event.queue_len_after, 2);
                assert_eq!(
                    event.format_version,
                    TestPma::SERIALIZED_MAINTENANCE_QUEUE_VERSION
                );
                assert!(event.persisted_bytes > 0);
            }
            other => panic!("expected maintenance-queue write event, got {other:?}"),
        }
        assert!(
            facade
                .formatted_last_write_event()
                .expect("formatted last write event")
                .contains("maintenance-queue-update action=Rebuild")
        );
    }

    #[test]
    fn facade_records_maintenance_queue_metrics() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(68u8);
        let dst = NodeId::from(69u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(69u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(70u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1007,
            EdgeEntry::new(
                NodeId::from(71u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(68u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(72u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1008,
            EdgeEntry::new(
                NodeId::from(68u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];

        assert_eq!(
            facade.rebuild_maintenance_queue(&[src.into(), dst.into()]),
            Some(1)
        );
        assert_eq!(
            facade.refresh_maintenance_queue(&[src.into(), dst.into()]),
            Some(1)
        );
        let summary = facade
            .run_queued_maintenance_cycles_with_segment_replacement_and_write(
                &[src.into(), dst.into()],
                &[vec![740, 741], vec![742]],
                &memory,
                230,
                1,
                0,
            )
            .expect("queued maintenance batch");
        assert_eq!(summary.cycles.len(), 1);

        let metrics = facade.production_metrics_snapshot();
        assert_eq!(metrics.maintenance_queue_rebuild_total, 1);
        assert_eq!(metrics.maintenance_queue_refresh_total, 1);
        assert_eq!(metrics.maintenance_queued_batch_total, 1);
        assert!(metrics.maintenance_queue_write_total > 0);
        assert!(metrics.maintenance_queue_last_persisted_bytes > 0);
        assert_eq!(
            metrics.maintenance_queue_format_version,
            TestPma::SERIALIZED_MAINTENANCE_QUEUE_VERSION
        );
    }

    #[test]
    fn facade_can_run_one_maintenance_cycle_with_segment_replacement_and_write() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(41u8);
        let dst = NodeId::from(42u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(42u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(43u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.forward.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            990,
            EdgeEntry::new(
                NodeId::from(44u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(41u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(45u8), crate::low_level::EdgeMeta::new(8, true)),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 1, 0);
        facade.graph.reverse.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(2), 0, EMPTY_LOG_OFFSET);
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            991,
            EdgeEntry::new(
                NodeId::from(41u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];

        let summary = facade
            .run_one_maintenance_cycle_with_segment_replacement_and_write(
                &[src.into(), dst.into()],
                &[vec![700, 701], vec![702]],
                &memory,
                151,
            )
            .expect("maintenance cycle write")
            .expect("maintenance summary");

        assert_eq!(summary.candidate.vertex_ref, src.into());
        assert_eq!(
            summary
                .rebalance
                .apply
                .segments
                .forward
                .new_segment
                .segment_id,
            1
        );
        assert!(!facade.graph.forward.0.has_dirty_regions());
        assert!(!facade.graph.reverse.0.has_dirty_regions());
        match facade.last_write_event() {
            Some(GraphStoreFacadeWriteEvent::MaintenanceCycle(event_summary)) => {
                let projection = GraphStoreMaintenanceCycleProjection::from_summary(event_summary);
                assert_eq!(projection.vertex_ref, src.into());
                assert_eq!(projection.window_start_ordinal, 0);
                assert_eq!(projection.window_end_ordinal_exclusive, 1);
                assert!(projection.priority_score > 0);
                assert!(projection.window_total_base_slots > 0);
            }
            other => panic!("expected maintenance-cycle write event, got {other:?}"),
        }
    }

    #[test]
    fn facade_can_run_maintenance_batch_with_segment_replacement_and_sweep() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let src = NodeId::from(51u8);
        let dst = NodeId::from(52u8);

        facade
            .bootstrap_vertex_refs_and_edges_and_write(&[src.into(), dst.into()], &[], &memory)
            .expect("bootstrap vertices");
        facade.set_insert_policy(GraphInsertPolicy {
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        });
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(52u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(NodeId::from(53u8), crate::low_level::EdgeMeta::new(8, true)),
            EdgeEntry::new(
                NodeId::from(54u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            EdgeEntry::new(
                NodeId::from(55u8),
                crate::low_level::EdgeMeta::new(10, true),
            ),
        ]);
        facade.graph.forward.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 2, 0);
        facade.graph.forward.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET);
        facade.graph.forward.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1090,
            EdgeEntry::new(
                NodeId::from(56u8),
                crate::low_level::EdgeMeta::new(11, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(
                NodeId::from(51u8),
                crate::low_level::EdgeMeta::new(7, false),
            ),
            EdgeEntry::new(
                NodeId::from(57u8),
                crate::low_level::EdgeMeta::new(8, false),
            ),
            EdgeEntry::new(
                NodeId::from(58u8),
                crate::low_level::EdgeMeta::new(9, false),
            ),
            EdgeEntry::new(
                NodeId::from(59u8),
                crate::low_level::EdgeMeta::new(10, true),
            ),
        ]);
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(EdgeIndex::new(0), 2, 0);
        facade.graph.reverse.0.vertices[1] =
            VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET);
        facade.graph.reverse.0.overflow_entries = vec![crate::low_level::OverflowEntry::new(
            1091,
            EdgeEntry::new(
                NodeId::from(51u8),
                crate::low_level::EdgeMeta::new(11, false),
            ),
            crate::low_level::LogOffset::EMPTY,
        )];
        let forward_segment = facade
            .manager
            .borrow_mut()
            .allocate_edge_segment(
                RegionKind::ForwardEdgeEntries,
                4,
                crate::low_level::EdgeSegmentState::Active,
            )
            .expect("allocate forward segment");
        let reverse_segment = facade
            .manager
            .borrow_mut()
            .allocate_edge_segment(
                RegionKind::ReverseEdgeEntries,
                4,
                crate::low_level::EdgeSegmentState::Active,
            )
            .expect("allocate reverse segment");
        facade.graph.forward.0.vertices[0] = VertexEntry::new(
            EdgeIndex::from(crate::low_level::EdgeRef::new(
                forward_segment.segment_id,
                0,
            )),
            2,
            0,
        );
        facade.graph.forward.0.vertices[1] = VertexEntry::new(
            EdgeIndex::from(crate::low_level::EdgeRef::new(
                forward_segment.segment_id,
                3,
            )),
            1,
            EMPTY_LOG_OFFSET,
        );
        facade.graph.reverse.0.vertices[0] = VertexEntry::new(
            EdgeIndex::from(crate::low_level::EdgeRef::new(
                reverse_segment.segment_id,
                0,
            )),
            2,
            0,
        );
        facade.graph.reverse.0.vertices[1] = VertexEntry::new(
            EdgeIndex::from(crate::low_level::EdgeRef::new(
                reverse_segment.segment_id,
                3,
            )),
            1,
            EMPTY_LOG_OFFSET,
        );
        let _ = facade
            .graph
            .sync_base_segment_capacities_from_manager(&facade.manager.borrow());

        let summary = facade
            .run_maintenance_cycles_with_segment_replacement_and_write(
                &[src.into(), dst.into()],
                &[vec![800, 801], vec![802]],
                &memory,
                160,
                1,
                0,
            )
            .expect("maintenance batch");

        assert_eq!(summary.cycles.len(), 1);
        assert_eq!(summary.swept_forward_segments.len(), 1);
        assert_eq!(summary.swept_reverse_segments.len(), 1);
        assert_projected_history(
            facade.write_history(),
            vec![
                GraphStoreWriteEventProjection::BootstrapGraph(
                    GraphStoreBootstrapGraphProjection {
                        vertex_ordinals: vec![
                            GraphStoreVertexOrdinalMapping {
                                vertex_ref: src.into(),
                                forward_ordinal: 0,
                                reverse_ordinal: 0,
                            },
                            GraphStoreVertexOrdinalMapping {
                                vertex_ref: dst.into(),
                                forward_ordinal: 1,
                                reverse_ordinal: 1,
                            },
                        ],
                        locators: Vec::new(),
                        refreshed: GraphStoreRefreshedVertices::new(Vec::new(), Vec::new()),
                    },
                ),
                GraphStoreWriteEventProjection::MaintenanceBatch(
                    GraphStoreMaintenanceBatchProjection {
                        cycles: 1,
                        queue_len_before: 0,
                        queue_len_after: 0,
                        swept_forward_segments: 1,
                        swept_reverse_segments: 1,
                        queue_storage_before: Some(GraphStoreMaintenanceQueueStorageProjection {
                            logical_len_bytes: 24,
                            queue_len: 0,
                            format_version: Some(1),
                            stored_checksum: None,
                            computed_checksum: None,
                            checksum_valid: Some(true),
                        }),
                        queue_storage_after: Some(GraphStoreMaintenanceQueueStorageProjection {
                            logical_len_bytes: 24,
                            queue_len: 0,
                            format_version: Some(1),
                            stored_checksum: None,
                            computed_checksum: None,
                            checksum_valid: Some(true),
                        }),
                    },
                ),
            ],
        );
    }

    #[test]
    fn facade_can_scan_property_index_directly_from_stable_memory_when_clean() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(51u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(888, "weight", &Value::Int64(5))
            .expect("set edge property");
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(mem_rc.as_ref())
            .expect("write all");

        assert_eq!(
            facade
                .try_scan_node_ids_by_property_eq_from_stable_memory(
                    facade.memory.as_ref(),
                    "uid",
                    &Value::Text("alice".into()),
                )
                .expect("scan node equality from stable memory"),
            vec![node_id]
        );
        assert_eq!(
            facade
                .try_scan_node_ids_by_property_from_stable_memory(facade.memory.as_ref(), "uid",)
                .expect("scan node property from stable memory"),
            vec![node_id]
        );
        assert_eq!(
            facade
                .try_scan_edge_ids_by_property_eq_from_stable_memory(
                    facade.memory.as_ref(),
                    "weight",
                    &Value::Int64(5),
                )
                .expect("scan edge equality from stable memory"),
            vec![888]
        );
        assert_eq!(
            facade
                .try_scan_edge_ids_by_property_from_stable_memory(facade.memory.as_ref(), "weight",)
                .expect("scan edge property from stable memory"),
            vec![888]
        );
    }

    #[test]
    fn facade_prefers_hydrated_property_index_when_dirty() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(61u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(999, "weight", &Value::Int64(7))
            .expect("set edge property");

        assert!(!facade.node_property_store_is_dirty());
        assert!(!facade.edge_property_store_is_dirty());

        let m = facade.memory.as_ref();
        assert_eq!(
            facade.scan_node_ids_by_property_eq_preferring_stable_memory(
                m,
                "uid",
                &Value::Text("alice".into()),
            ),
            vec![node_id]
        );
        assert_eq!(
            facade.scan_node_ids_by_property_preferring_stable_memory(m, "uid"),
            vec![node_id]
        );
        assert_eq!(
            facade.scan_edge_ids_by_property_eq_preferring_stable_memory(
                m,
                "weight",
                &Value::Int64(7),
            ),
            vec![999]
        );
        assert_eq!(
            facade.scan_edge_ids_by_property_preferring_stable_memory(m, "weight"),
            vec![999]
        );
    }

    #[test]
    fn facade_property_index_hydrate_uses_fixed_slot_equality_map() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(41u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(777, "weight", &Value::Int64(5))
            .expect("set edge property");
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(mem_rc.as_ref())
            .expect("write all");

        let slots = facade.fixed_memory_slots();
        let equality_map = open_fixed_slot_property_equality_map(&slots);
        let snapshot = snapshot_fixed_slot_property_equality_map(&equality_map);
        assert_eq!(
            snapshot
                .iter()
                .filter(|entry| matches!(
                    entry.key().entity_kind,
                    PropertyIndexEntityKind::VertexNode
                ))
                .count(),
            1
        );
        assert_eq!(
            snapshot
                .iter()
                .filter(|entry| matches!(
                    entry.key().entity_kind,
                    PropertyIndexEntityKind::VertexEdge
                ))
                .count(),
            1
        );

        let rehydrated = GraphStore::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            (*facade.memory).clone(),
        )
        .unwrap();
        assert_eq!(
            rehydrated.scan_node_ids_by_property_eq("uid", &Value::Text("alice".into())),
            vec![node_id]
        );
        assert_eq!(
            rehydrated.scan_edge_ids_by_property_eq("weight", &Value::Int64(5)),
            vec![777]
        );
    }

    #[test]
    fn facade_batch_session_supports_mixed_mutation_flow() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade
            .graph
            .insert_base_edge_pair(
                77,
                NodeId::from(1u8).into(),
                0,
                NodeId::from(2u8).into(),
                0,
                7u16.into(),
            )
            .expect("seed sidecar");

        let mem_rc = Rc::clone(&facade.memory);
        let mut adapter = GraphStoreStoreAdapter::new(&mut facade, mem_rc.as_ref());
        let mut batch = adapter.begin_batch_mutation();
        let replaced = batch
            .replace_edge_pair(EdgeReplaceSpec {
                edge_id: 77,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: NodeId::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: NodeId::from(3u8).into(),
                    dst_ordinal: 0,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::base(SurfaceKind::Forward, NodeId::from(1u8), 0),
                    reverse: LogicalEdgeLocator::base(SurfaceKind::Reverse, NodeId::from(3u8), 0),
                },
                edge_meta: 9u16.into(),
            })
            .expect("replace");
        assert_eq!(replaced.0, GraphMutationPath::Base);

        let tombstoned = batch
            .tombstone_edge_pair(EdgeTombstoneSpec {
                edge_id: 77,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: NodeId::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: NodeId::from(3u8).into(),
                    dst_ordinal: 0,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::base(SurfaceKind::Forward, NodeId::from(1u8), 0),
                    reverse: LogicalEdgeLocator::base(SurfaceKind::Reverse, NodeId::from(3u8), 0),
                },
            })
            .expect("tombstone");
        assert_eq!(tombstoned, GraphMutationPath::Base);

        let refreshed = batch.flush().expect("flush");
        assert!(refreshed.0.contains(&0));
        assert!(refreshed.1.contains(&0));
    }

    #[test]
    fn facade_replace_and_tombstone_convenience_methods_write_back() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade
            .graph
            .insert_base_edge_pair(
                77,
                NodeId::from(1u8).into(),
                0,
                NodeId::from(2u8).into(),
                0,
                7u16.into(),
            )
            .expect("seed sidecar");

        let replace_summary: GraphStoreMutationWriteSummary<_> = facade
            .replace_edge_pair_and_write(
                EdgeReplaceSpec {
                    edge_id: 77,
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: NodeId::from(1u8).into(),
                        src_ordinal: 0,
                        dst_vertex_ref: NodeId::from(3u8).into(),
                        dst_ordinal: 0,
                    },
                    locators: EdgePairLogicalLocators {
                        forward: LogicalEdgeLocator::base(
                            SurfaceKind::Forward,
                            NodeId::from(1u8),
                            0,
                        ),
                        reverse: LogicalEdgeLocator::base(
                            SurfaceKind::Reverse,
                            NodeId::from(3u8),
                            0,
                        ),
                    },
                    edge_meta: 9u16.into(),
                },
                &memory,
            )
            .expect("replace and write");
        assert_eq!(replace_summary.mutation.0, GraphMutationPath::Base);

        let tombstone_summary = facade
            .tombstone_edge_pair_and_write(
                EdgeTombstoneSpec {
                    edge_id: 77,
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: NodeId::from(1u8).into(),
                        src_ordinal: 0,
                        dst_vertex_ref: NodeId::from(3u8).into(),
                        dst_ordinal: 0,
                    },
                    locators: EdgePairLogicalLocators {
                        forward: LogicalEdgeLocator::base(
                            SurfaceKind::Forward,
                            NodeId::from(1u8),
                            0,
                        ),
                        reverse: LogicalEdgeLocator::base(
                            SurfaceKind::Reverse,
                            NodeId::from(3u8),
                            0,
                        ),
                    },
                },
                &memory,
            )
            .expect("tombstone and write");
        assert_eq!(tombstone_summary.mutation, GraphMutationPath::Base);
        assert!(tombstone_summary.refreshed.forward.contains(&0));
        assert!(tombstone_summary.refreshed.reverse.contains(&0));
    }

    #[test]
    fn facade_try_rebuild_logical_locator_sidecar_rejects_mismatched_inputs() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = GraphStore::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let err = facade
            .try_rebuild_logical_locator_sidecar(&[NodeId::from(1u8).into()], &[])
            .expect_err("mismatched ids should fail");
        assert_eq!(err, GraphStoreError::InvalidLocatorInputs);
    }

    #[test]
    fn facade_can_hydrate_with_logical_locator_sidecar_in_one_step() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = GraphStore::hydrate_from_stable_memory_with_logical_locator_sidecar(
            manager,
            memory,
            &[NodeId::from(1u8).into()],
            &[vec![77]],
        )
        .expect("hydrate with logical locator sidecar");

        assert_eq!(
            facade.graph.logical_locator(77),
            Some(crate::low_level::LogicalEdgeLocator::base(
                crate::low_level::SurfaceKind::Forward,
                NodeId::from(1u8),
                0,
            ))
        );
    }

    #[test]
    fn facade_can_bootstrap_empty_graph() {
        let memory = VecMemory::default();
        let facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap empty");

        assert!(facade.graph.forward.0.vertices.is_empty());
        assert!(facade.graph.forward.0.base_entries.is_empty());
        assert!(facade.graph.reverse.0.vertices.is_empty());
        assert!(facade.graph.reverse.0.base_entries.is_empty());
        assert!(
            facade
                .manager
                .borrow()
                .layout
                .region(RegionKind::ForwardVertexTable)
                .is_some()
        );
        assert!(
            facade
                .manager
                .borrow()
                .layout
                .region(RegionKind::ReverseSegmentLog)
                .is_some()
        );
    }

    #[test]
    fn facade_can_append_empty_vertex_pair_and_write() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");

        let summary: GraphStoreAppendVertexWriteSummary = facade
            .append_empty_vertex_pair_and_write(&memory)
            .expect("append empty vertex pair");

        assert_eq!(summary.ordinals, (0, 0));
        assert_eq!(
            summary.refreshed,
            GraphStoreRefreshedVertices::new(Vec::new(), Vec::new())
        );
        assert_eq!(facade.graph.forward.0.vertices.len(), 1);
        assert_eq!(facade.graph.reverse.0.vertices.len(), 1);
        assert_projected_history(
            facade.write_history(),
            vec![GraphStoreWriteEventProjection::BootstrapVertices(
                GraphStoreBootstrapVerticesProjection::from_single_summary(&summary),
            )],
        );
    }

    #[test]
    fn facade_can_append_multiple_empty_vertex_pairs_and_write() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");

        let summary: GraphStoreAppendVerticesWriteSummary = facade
            .append_empty_vertex_pairs_and_write(3, &memory)
            .expect("append empty vertex pairs");

        assert_eq!(summary.ordinals, vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(
            summary.refreshed,
            GraphStoreRefreshedVertices::new(Vec::new(), Vec::new())
        );
        assert_eq!(facade.graph.forward.0.vertices.len(), 3);
        assert_eq!(facade.graph.reverse.0.vertices.len(), 3);
    }

    #[test]
    fn facade_can_bootstrap_first_edge_between_new_vertices() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");

        let summary: GraphStoreBootstrapEdgeWriteSummary = facade
            .bootstrap_edge_between_new_vertices_and_write(
                77,
                NodeId::from(1u8),
                NodeId::from(2u8),
                9,
                &memory,
            )
            .expect("bootstrap first edge");

        assert_eq!(summary.ordinals, (0, 1));
        let GraphInsertResult::Inserted { .. } = summary.insert else {
            panic!("expected inserted result");
        };
        assert_eq!(facade.graph.forward.0.vertices.len(), 2);
        assert_eq!(facade.graph.reverse.0.vertices.len(), 2);
        assert_eq!(
            facade.shared_write_history(),
            vec![GraphStoreWriteEventProjection::BootstrapEdge(
                GraphStoreBootstrapEdgeProjection::from_facade_summary(&summary),
            )]
        );
        assert_eq!(
            facade.graph.logical_locator(77),
            Some(crate::low_level::LogicalEdgeLocator::base(
                crate::low_level::SurfaceKind::Forward,
                NodeId::from(1u8),
                0,
            ))
        );
    }

    #[test]
    fn facade_can_bootstrap_multiple_vertices_and_edges() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");

        let summary: GraphStoreBootstrapGraphWriteSummary = facade
            .bootstrap_vertex_refs_and_edges_and_write(
                &[
                    NodeId::from(1u8).into(),
                    NodeId::from(2u8).into(),
                    NodeId::from(3u8).into(),
                ],
                &[(77, 0, 1, 9), (88, 1, 2, 11)],
                &memory,
            )
            .expect("bootstrap graph");

        assert_eq!(
            summary.vertex_ordinals,
            vec![
                GraphStoreVertexOrdinalMapping {
                    vertex_ref: NodeId::from(1u8).into(),
                    forward_ordinal: 0,
                    reverse_ordinal: 0,
                },
                GraphStoreVertexOrdinalMapping {
                    vertex_ref: NodeId::from(2u8).into(),
                    forward_ordinal: 1,
                    reverse_ordinal: 1,
                },
                GraphStoreVertexOrdinalMapping {
                    vertex_ref: NodeId::from(3u8).into(),
                    forward_ordinal: 2,
                    reverse_ordinal: 2,
                },
            ]
        );
        assert_eq!(summary.inserts.len(), 2);
        assert_eq!(
            summary.locators,
            vec![
                GraphStoreEdgeLogicalLocatorMapping {
                    edge_id: 77,
                    canonical: crate::low_level::LogicalEdgeLocator::base(
                        crate::low_level::SurfaceKind::Forward,
                        NodeId::from(1u8),
                        0,
                    ),
                    forward: crate::low_level::LogicalEdgeLocator::base(
                        crate::low_level::SurfaceKind::Forward,
                        NodeId::from(1u8),
                        0,
                    ),
                    reverse: crate::low_level::LogicalEdgeLocator::base(
                        crate::low_level::SurfaceKind::Reverse,
                        NodeId::from(2u8),
                        0,
                    ),
                },
                GraphStoreEdgeLogicalLocatorMapping {
                    edge_id: 88,
                    canonical: crate::low_level::LogicalEdgeLocator::overflow(
                        crate::low_level::SurfaceKind::Forward,
                        NodeId::from(2u8),
                        0,
                    ),
                    forward: crate::low_level::LogicalEdgeLocator::overflow(
                        crate::low_level::SurfaceKind::Forward,
                        NodeId::from(2u8),
                        0,
                    ),
                    reverse: crate::low_level::LogicalEdgeLocator::overflow(
                        crate::low_level::SurfaceKind::Reverse,
                        NodeId::from(3u8),
                        0,
                    ),
                },
            ]
        );
        assert_eq!(facade.graph.forward.0.vertices.len(), 3);
        assert_eq!(facade.graph.reverse.0.vertices.len(), 3);
        assert_eq!(
            facade.graph.logical_locator(77),
            Some(crate::low_level::LogicalEdgeLocator::base(
                crate::low_level::SurfaceKind::Forward,
                NodeId::from(1u8),
                0,
            ))
        );
        assert_eq!(
            facade.graph.logical_locator(88),
            Some(crate::low_level::LogicalEdgeLocator::overflow(
                crate::low_level::SurfaceKind::Forward,
                NodeId::from(2u8),
                0,
            ))
        );
    }

    #[test]
    fn facade_implements_graph_pma_store_trait() {
        fn touch_store(
            store: &mut impl GraphStoreStore,
            memory: &impl Memory,
        ) -> GraphStoreResult<(usize, usize, usize, usize)> {
            let _ = store.manager();
            let _ = store.graph();
            let _ = store.append_empty_vertex_pair()?;
            let _ = store.append_empty_vertex_pairs(1)?;
            let bootstrap = store.bootstrap_vertex_refs_and_edges_and_write(
                &[NodeId::from(11u8).into(), NodeId::from(12u8).into()],
                &[(900, 0, 1, 3)],
                memory,
            )?;
            store.try_refresh_and_write_dirty_to_stable_memory(memory)?;
            Ok((
                store.graph().forward.0.vertices.len(),
                store.graph().reverse.0.vertices.len(),
                bootstrap.inserts.len(),
                store.write_history().len(),
            ))
        }

        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let counts = touch_store(&mut facade, mem_rc.as_ref()).expect("touch via trait");
        assert_eq!(counts, (4, 4, 1, 1));
        assert_eq!(
            GraphStoreStore::formatted_last_write_event(&facade),
            Some("bootstrap-graph vertices=2 edges=1 refreshed=(1,1) fwd=[2] rev=[3]".to_owned())
        );
    }

    #[test]
    fn graph_pma_store_adapter_can_bootstrap_via_trait_boundary() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let mut adapter = facade.bind(mem_rc.as_ref());

        let summary = adapter
            .bootstrap_vertex_refs_and_edges(
                &[NodeId::from(21u8).into(), NodeId::from(22u8).into()],
                &[(901, 0, 1, 5)],
            )
            .expect("bootstrap through adapter");

        assert_eq!(summary.vertex_ordinals.len(), 2);
        assert_eq!(summary.inserts.len(), 1);
        assert_eq!(summary.locators.len(), 1);
        assert!(matches!(
            adapter.last_write_event(),
            Some(GraphStoreFacadeWriteEvent::BootstrapGraph(_))
        ));
        let event_projection = match adapter.last_write_event() {
            Some(GraphStoreFacadeWriteEvent::BootstrapGraph(event_summary)) => {
                event_summary.projection()
            }
            other => panic!("expected bootstrap graph event, got {other:?}"),
        };
        assert_eq!(summary.projection(), event_projection);
        assert!(matches!(
            adapter.write_history(),
            [GraphStoreFacadeWriteEvent::BootstrapGraph(_)]
        ));
        assert_eq!(
            GraphStoreStore::formatted_last_write_event(adapter.store),
            Some("bootstrap-graph vertices=2 edges=1 refreshed=(1,1) fwd=[0] rev=[1]".to_owned())
        );
    }

    #[test]
    fn graph_pma_store_adapter_can_replace_and_tombstone_edges() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let mut adapter = facade.bind(mem_rc.as_ref());

        let bootstrap = adapter
            .bootstrap_vertex_refs_and_edges(
                &[NodeId::from(31u8).into(), NodeId::from(32u8).into()],
                &[(902, 0, 1, 5)],
            )
            .expect("bootstrap through adapter");

        let src = bootstrap.vertex_ordinals[0];
        let dst = bootstrap.vertex_ordinals[1];

        let replaced = adapter
            .replace_edge_pair(EdgeReplaceSpec {
                edge_id: 902,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: src.vertex_ref,
                    src_ordinal: src.forward_ordinal,
                    dst_vertex_ref: NodeId::from(33u8).into(),
                    dst_ordinal: dst.reverse_ordinal,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::base(SurfaceKind::Forward, src.vertex_ref, 0),
                    reverse: LogicalEdgeLocator::base(SurfaceKind::Reverse, NodeId::from(33u8), 0),
                },
                edge_meta: 7u16.into(),
            })
            .expect("replace through adapter");
        assert_eq!(replaced.mutation.0, GraphMutationPath::Base);
        let replace_projection = adapter
            .last_write_event()
            .and_then(GraphStoreFacadeWriteEvent::edge_projection)
            .expect("replace edge projection");
        assert_eq!(
            replace_projection.operation,
            GraphStoreEdgeWriteOperation::ReplaceLabel
        );
        assert_eq!(replace_projection.path, GraphMutationPath::Base);

        let tombstoned = adapter
            .tombstone_edge_pair(EdgeTombstoneSpec {
                edge_id: 902,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: src.vertex_ref,
                    src_ordinal: src.forward_ordinal,
                    dst_vertex_ref: NodeId::from(33u8).into(),
                    dst_ordinal: dst.reverse_ordinal,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::base(SurfaceKind::Forward, src.vertex_ref, 0),
                    reverse: LogicalEdgeLocator::base(SurfaceKind::Reverse, NodeId::from(33u8), 0),
                },
            })
            .expect("tombstone through adapter");
        assert_eq!(tombstoned.mutation, GraphMutationPath::Base);
        let delete_projection = adapter
            .last_write_event()
            .and_then(GraphStoreFacadeWriteEvent::edge_projection)
            .expect("delete edge projection");
        assert_eq!(
            delete_projection.operation,
            GraphStoreEdgeWriteOperation::Delete
        );
        assert_eq!(delete_projection.path, GraphMutationPath::Base);
        assert_eq!(
            adapter.shared_write_history(),
            vec![
                GraphStoreWriteEventProjection::BootstrapGraph(bootstrap.projection()),
                GraphStoreWriteEventProjection::Edge(replace_projection),
                GraphStoreWriteEventProjection::Edge(delete_projection),
            ]
        );
        assert_eq!(
            adapter.formatted_write_history(),
            vec![
                "bootstrap-graph vertices=2 edges=1 refreshed=(1,1) fwd=[0] rev=[1]".to_owned(),
                "edge operation=ReplaceLabel path=Base refreshed=(1,1) fwd=[0] rev=[1]".to_owned(),
                "edge operation=Delete path=Base refreshed=(1,1) fwd=[0] rev=[1]".to_owned(),
            ]
        );
        assert_eq!(
            adapter.formatted_last_write_event(),
            Some("edge operation=Delete path=Base refreshed=(1,1) fwd=[0] rev=[1]".to_owned())
        );
    }

    #[test]
    fn graph_pma_store_adapter_can_start_batch_session() {
        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let mut adapter = facade.bind(mem_rc.as_ref());

        let mut batch = adapter.begin_batch_mutation();
        let refreshed = batch.flush().expect("flush empty batch");
        assert_eq!(refreshed, (Vec::new(), Vec::new()));
    }

    #[test]
    fn graph_pma_service_trait_can_drive_bootstrap_and_flush() {
        fn use_service(
            service: &mut impl GraphStoreService,
        ) -> GraphStoreResult<(usize, usize, bool, GraphStoreBootstrapGraphProjection)> {
            let summary = service.bootstrap_vertex_refs_and_edges(
                &[NodeId::from(41u8).into(), NodeId::from(42u8).into()],
                &[(903, 0, 1, 13)],
            )?;
            let projection = summary.projection();
            let _ = service.flush_dirty()?;
            Ok((
                summary.inserts.len(),
                service.write_history().len(),
                matches!(
                    service.last_write_event(),
                    Some(GraphStoreFacadeWriteEvent::BootstrapGraph(_))
                ),
                projection,
            ))
        }

        let memory = VecMemory::default();
        let mut facade = GraphStore::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let mut adapter = facade.bind(mem_rc.as_ref());
        let (insert_count, history_len, has_insert_event, summary_projection) =
            use_service(&mut adapter).expect("drive via service trait");
        assert_eq!(insert_count, 1);
        assert_eq!(history_len, 1);
        assert!(has_insert_event);
        let event_projection = match adapter.last_write_event() {
            Some(GraphStoreFacadeWriteEvent::BootstrapGraph(event_summary)) => {
                event_summary.projection()
            }
            other => panic!("expected bootstrap graph event, got {other:?}"),
        };
        assert_eq!(summary_projection, event_projection);
        assert_eq!(
            GraphStoreService::formatted_last_write_event(&adapter),
            Some("bootstrap-graph vertices=2 edges=1 refreshed=(1,1) fwd=[0] rev=[1]".to_owned())
        );
    }
}

pub mod experimental_dgap {
    //! Fixed [`MemoryId`](ic_stable_structures::memory_manager::MemoryId) slots for the
    //! phase-1 `graph-store` layout.
    //!
    //! This is the intended `MemoryManager` layout for the `ic-stable-csr`
    //! adjacency backend plus the remaining property/index regions. The active
    //! facade path still uses the legacy low-level stack for property/index,
    //! but new integrations should treat these ids as canonical.
    pub use crate::{
        GRAPH_STORE_FIXED_MEMORY_IDS, GRAPH_STORE_MEMORY_ID_ADJACENCY_GC_QUEUE,
        GRAPH_STORE_MEMORY_ID_DELETED_VERTICES, GRAPH_STORE_MEMORY_ID_EDGE_PROPERTY_STORE,
        GRAPH_STORE_MEMORY_ID_FORWARD_EDGES_AND_LOG,
        GRAPH_STORE_MEMORY_ID_FORWARD_SEGMENT_EDGE_COUNTS,
        GRAPH_STORE_MEMORY_ID_FORWARD_VERTEX_TABLE, GRAPH_STORE_MEMORY_ID_GC_STATE,
        GRAPH_STORE_MEMORY_ID_LABEL_CATALOG, GRAPH_STORE_MEMORY_ID_MAINTENANCE_QUEUE,
        GRAPH_STORE_MEMORY_ID_NODE_PROPERTY_STORE,
        GRAPH_STORE_MEMORY_ID_PROPERTY_INDEX, GRAPH_STORE_MEMORY_ID_REVERSE_EDGES_AND_LOG,
        GRAPH_STORE_MEMORY_ID_REVERSE_SEGMENT_EDGE_COUNTS,
        GRAPH_STORE_MEMORY_ID_REVERSE_VERTEX_TABLE, GRAPH_STORE_MEMORY_ID_SHARD_CANISTER_DIRECTORY,
        graph_store_fixed_memory_ids,
    };
    use ic_stable_structures::memory_manager::MemoryId;

    /// The seven adjacency-related memories used by the Phase-1
    /// `CsrGraphWithGcQueueSparseDeleted` backend.
    #[inline]
    pub fn ic_stable_csr_memory_ids() -> (
        MemoryId,
        MemoryId,
        MemoryId,
        MemoryId,
        MemoryId,
        MemoryId,
        MemoryId,
        MemoryId,
    ) {
        (
            GRAPH_STORE_MEMORY_ID_FORWARD_VERTEX_TABLE,
            GRAPH_STORE_MEMORY_ID_REVERSE_VERTEX_TABLE,
            GRAPH_STORE_MEMORY_ID_FORWARD_SEGMENT_EDGE_COUNTS,
            GRAPH_STORE_MEMORY_ID_FORWARD_EDGES_AND_LOG,
            GRAPH_STORE_MEMORY_ID_REVERSE_SEGMENT_EDGE_COUNTS,
            GRAPH_STORE_MEMORY_ID_REVERSE_EDGES_AND_LOG,
            GRAPH_STORE_MEMORY_ID_DELETED_VERTICES,
            GRAPH_STORE_MEMORY_ID_ADJACENCY_GC_QUEUE,
        )
    }
}
