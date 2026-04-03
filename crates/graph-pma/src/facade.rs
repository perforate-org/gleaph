//! Thin rewrite-facing facade over the new low-level `graph-pma` runtime.
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

use std::cell::{Ref, RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::VecMemory;
use crate::stable::Memory;
use gleaph_gql::Value;
use gleaph_graph_kernel::{EdgeId, LabelId, NodeId, PropertyMap};

use crate::low_level::{
    BucketSizeInPages, EdgeEntry, EdgeReplaceSpec, EdgeTombstoneSpec, ExtentChain,
    ExtentGrowthPolicy, ExtentGrowthRequest, ExtentId, ForwardSurfaceRuntime,
    GraphBatchMutationSession, GraphEnsureCapacitySegmentWriteSummary,
    GraphEnsureCapacityWriteSummary, GraphInsertPolicy, GraphInsertResult,
    GraphInsertSegmentWriteSummary, GraphInsertWriteSummary, GraphMaintenanceBatchWriteSummary,
    GraphMaintenanceCandidate, GraphMaintenanceCyclePlan, GraphMaintenanceCycleWriteSummary,
    GraphMaintenanceWorkItem, GraphMutationPath, GraphRuntime, HydratedSurfaceRuntimes,
    HydrationError, LogicalEdgeLocator, RebalanceInsertSpec, RebalancePrepareSpec, RegionKind,
    RegionManager, ResolvedEdgeSlot, ReverseSurfaceRuntime, SurfaceVertexWindowReserveHint,
    SurfaceVertexWindowSummary, VertexEntry, VertexRef, WasmPages, WritebackError,
    estimate_vertex_window_reserve_hint_from_stable_memory, forward_surface_from_layout,
    hydrate_surface_runtimes_from_stable_memory, read_edge_entries_by_ref_from_stable_memory,
    read_vertex_base_edge_ref_from_stable_memory, read_vertex_base_entries_from_stable_memory,
    read_vertex_base_entry_from_stable_memory, read_vertex_entries_from_stable_memory,
    read_vertex_entry_by_ref_from_stable_memory, read_vertex_entry_from_stable_memory,
    read_vertex_reserved_base_entries_from_stable_memory,
    read_vertex_reserved_span_len_from_stable_memory, reverse_surface_from_layout,
    summarize_vertex_window_from_stable_memory, write_surface_runtimes_to_stable_memory,
};
use crate::observability::{
    format_last_write_event, format_maintenance_queue, format_maintenance_queue_storage,
    format_write_event_history,
};
use crate::property_index::{
    PropertyEqualityInplaceMap, PropertyIndex, PropertyIndexEntityKind, PropertyIndexEntry,
    PropertyIndexError, PropertyIndexKey, PropertyIndexNodeStoreMutationKind,
    empty_property_equality_inplace_map, ensure_pidx_v3_btree_subregion_for_hydrate,
    hydrate_property_equality_inplace_map, read_pidx_v3_header_from_stable_memory,
    scan_edge_property_index_property_prefix_from_stable_memory,
    scan_edge_property_index_value_prefix_from_stable_memory,
    scan_node_property_index_property_prefix_from_stable_memory,
    scan_node_property_index_value_prefix_from_stable_memory, snapshot_from_equality_any_memory,
    write_property_index_stable_equality_to_stable_memory,
};
use crate::property_store::{
    GraphPropertyStableMap, PropertyStoreError, btree_distinct_property_names,
    btree_get_edge_property, btree_get_node_property, btree_scan_entities,
    btree_scan_entities_property_subset, btree_scan_entity, default_property_region_chain,
    empty_graph_property_stable_map, load_graph_property_stable_map_from_stable_memory,
};
pub use errors::{RewriteGraphPmaError, RewriteGraphPmaResult};
pub use facade_types::{
    PropertyIndexFallbackReason, RewriteAppendVertexWriteSummary,
    RewriteAppendVerticesWriteSummary, RewriteBootstrapEdgeProjection,
    RewriteBootstrapEdgeWriteSummary, RewriteBootstrapGraphProjection,
    RewriteBootstrapGraphWriteSummary, RewriteBootstrapVerticesProjection,
    RewriteEdgeLogicalLocatorMapping, RewriteEdgeWriteOperation, RewriteEdgeWriteProjection,
    RewriteEnsureCapacityProjection, RewriteFacadeWriteEvent, RewriteGraphMutationWriteSummary,
    RewriteInsertEdgeProjection, RewriteMaintenanceBatchProjection,
    RewriteMaintenanceCycleProjection, RewriteMaintenanceQueueAction,
    RewriteMaintenanceQueueItemProjection, RewriteMaintenanceQueueProjection,
    RewriteMaintenanceQueueStorageProjection, RewriteNodeDeleteProjection,
    RewriteProductionMetrics, RewriteProductionMetricsSnapshot,
    RewritePropertyIndexMutationSummary, RewritePropertyIndexTouchedSections,
    RewritePropertyMutationWriteSummary, RewritePropertyWriteProjection, RewriteRefreshedVertices,
    RewriteVertexOrdinalMapping, RewriteWriteEventProjection,
};

type RewriteReplaceEdgeSummary =
    RewriteGraphMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>;

#[cfg(test)]
mod property_index_page_size_test_hook {
    use std::cell::Cell;

    thread_local! {
        static OVERRIDE: Cell<Option<u32>> = const { Cell::new(None) };
    }

    #[allow(dead_code)] // Used by tests that override PIDX logical page size via `set(Some(..))`.
    pub fn set(v: Option<u32>) {
        OVERRIDE.set(v);
    }

    pub fn get() -> Option<u32> {
        OVERRIDE.get()
    }
}

/// When true, the next node-side property index mutation path returns
/// [`PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage`] (test-only).
#[cfg(test)]
pub(crate) static FAIL_NEXT_NODE_PROPERTY_INDEX_SYNC_TEST: AtomicBool = AtomicBool::new(false);

/// When true, the next edge-side property index mutation path returns
/// [`PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage`] (test-only).
#[cfg(test)]
pub(crate) static FAIL_NEXT_EDGE_PROPERTY_INDEX_SYNC_TEST: AtomicBool = AtomicBool::new(false);

/// Thin entrypoint for the rewrite implementation of `graph-pma`.
///
/// This facade owns the region-manager metadata together with the hydrated
/// graph runtime, while keeping stable-memory access explicit at method call
/// sites. The goal is to make the rewrite usable without hiding the
/// low-level-first model we are still iterating on.
pub struct RewriteGraphPma<M: Memory = VecMemory> {
    /// Region metadata and allocator-side state for the rewrite.
    pub manager: Rc<RefCell<RegionManager>>,
    /// Canonical stable-memory backing (shared with PIDX btree subregion I/O).
    pub memory: Rc<RefCell<M>>,
    /// In-memory forward/reverse adjacency runtime plus locator sidecar.
    pub graph: GraphRuntime,
    /// Node properties: `PSB1` header + [`StableBTreeMap`] (`StableBTreeMap` in stable memory).
    pub node_property_store: GraphPropertyStableMap<M>,
    /// Edge properties: same layout as [`Self::node_property_store`].
    pub edge_property_store: GraphPropertyStableMap<M>,
    /// Btree payload byte length for [`Self::node_property_store`].
    pub node_property_btree_payload: Rc<RefCell<u64>>,
    /// Btree payload byte length for [`Self::edge_property_store`].
    pub edge_property_btree_payload: Rc<RefCell<u64>>,
    /// Derived equality index for node properties.
    pub node_property_index: PropertyIndex,
    /// Derived equality index for edge properties.
    pub edge_property_index: PropertyIndex,
    /// Stable B-tree backing the persisted node + edge property equality index (PIDX v3).
    pub property_equality_map: PropertyEqualityInplaceMap<M>,
    /// Byte length of the btree payload (after the PIDX v3 header); kept in sync with the subregion memory.
    pub property_index_btree_payload: Rc<RefCell<u64>>,
    /// When set, the next PIDX flush must sync the v3 header with [`Self::property_index_btree_payload`].
    pub property_index_dirty: bool,
    /// Whether the node property region header may be out of sync with the btree length cell.
    pub node_property_store_dirty: bool,
    /// Whether the edge property region header may be out of sync with the btree length cell.
    pub edge_property_store_dirty: bool,
    /// Most recent facade-level write event.
    pub last_write_event: Option<RewriteFacadeWriteEvent>,
    /// Recent facade-level write events in observation order.
    pub write_history: Vec<RewriteFacadeWriteEvent>,
    /// In-process production-facing metrics for property/index paths.
    pub production_metrics: RewriteProductionMetrics,
}

impl<M: Memory> std::fmt::Debug for RewriteGraphPma<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RewriteGraphPma")
            .field("manager", &self.manager)
            .field("memory", &"...")
            .field("graph", &self.graph)
            .field("node_property_store_len", &self.node_property_store.len())
            .field("edge_property_store_len", &self.edge_property_store.len())
            .field("node_property_index", &self.node_property_index)
            .field("edge_property_index", &self.edge_property_index)
            .field(
                "property_equality_map_len",
                &self.property_equality_map.len(),
            )
            .field("property_index_dirty", &self.property_index_dirty)
            .field("node_property_store_dirty", &self.node_property_store_dirty)
            .field("edge_property_store_dirty", &self.edge_property_store_dirty)
            .field("last_write_event", &self.last_write_event)
            .field("write_history_len", &self.write_history.len())
            .field("production_metrics", &self.production_metrics)
            .finish()
    }
}

impl<M: Memory + Clone> Clone for RewriteGraphPma<M> {
    fn clone(&self) -> Self {
        let manager = Rc::new(RefCell::new(self.manager.borrow().clone()));
        let memory = Rc::new(RefCell::new(self.memory.borrow().clone()));
        let btree_payload = Rc::new(RefCell::new(*self.property_index_btree_payload.borrow()));
        let property_equality_map = empty_property_equality_inplace_map(
            Rc::clone(&manager),
            Rc::clone(&memory),
            Rc::clone(&btree_payload),
        );
        let node_pl = Rc::new(RefCell::new(*self.node_property_btree_payload.borrow()));
        let edge_pl = Rc::new(RefCell::new(*self.edge_property_btree_payload.borrow()));
        let node_property_store = empty_graph_property_stable_map(
            Rc::clone(&manager),
            Rc::clone(&memory),
            Rc::clone(&node_pl),
            RegionKind::NodePropertyStore,
        );
        let edge_property_store = empty_graph_property_stable_map(
            Rc::clone(&manager),
            Rc::clone(&memory),
            Rc::clone(&edge_pl),
            RegionKind::EdgePropertyStore,
        );
        let mut clone = Self {
            manager,
            memory,
            graph: self.graph.clone(),
            node_property_store,
            edge_property_store,
            node_property_btree_payload: node_pl,
            edge_property_btree_payload: edge_pl,
            node_property_index: self.node_property_index.clone(),
            edge_property_index: self.edge_property_index.clone(),
            property_equality_map,
            property_index_btree_payload: btree_payload,
            property_index_dirty: self.property_index_dirty,
            node_property_store_dirty: self.node_property_store_dirty,
            edge_property_store_dirty: self.edge_property_store_dirty,
            last_write_event: self.last_write_event.clone(),
            write_history: self.write_history.clone(),
            production_metrics: self.production_metrics.clone(),
        };
        for e in self.property_equality_map.iter() {
            clone
                .property_equality_map
                .insert(e.key().clone(), e.value().clone());
        }
        for e in self.node_property_store.iter() {
            clone
                .node_property_store
                .insert(e.key().clone(), e.value().clone());
        }
        for e in self.edge_property_store.iter() {
            clone
                .edge_property_store
                .insert(e.key().clone(), e.value().clone());
        }
        clone
    }
}

/// Thin facade-level batch mutation session.
///
/// This wraps the low-level `GraphBatchMutationSession` so callers that start
/// from `RewriteGraphPma` do not need to wire the manager and graph runtime
/// manually for each batch.
pub struct RewriteGraphPmaBatchSession<'a, M: Memory> {
    inner: GraphBatchMutationSession<'a, M>,
}

/// Thin higher-level adapter that binds one [`RewriteGraphStore`] together with
/// one stable-memory handle.
///
/// This keeps upper layers from threading `memory` through every facade call
/// while still reusing the rewrite-facing trait boundary instead of depending
/// directly on [`RewriteGraphPma`].
pub struct RewriteGraphStoreAdapter<'a, S: RewriteGraphStore> {
    store: &'a mut S,
    memory: &'a S::Mem,
}

/// Thin higher-level service boundary over one bound rewrite graph store.
///
/// Unlike [`RewriteGraphStore`], this trait assumes stable memory is already
/// bound, so upper layers can express bootstrap and mutation flows without
/// threading a `Memory` handle through every call.
pub trait RewriteGraphService {
    /// Returns the most recent facade-level write event observed through this service.
    fn last_write_event(&self) -> Option<&RewriteFacadeWriteEvent>;

    /// Returns recent facade-level write events in observation order.
    fn write_history(&self) -> &[RewriteFacadeWriteEvent];

    /// Returns recent facade-level write events projected into shared diagnostics history.
    fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(
            &self
                .write_history()
                .iter()
                .flat_map(RewriteFacadeWriteEvent::shared_projections)
                .collect::<Vec<_>>(),
        )
    }

    /// Returns the most recent facade-level write event projected into one diagnostics line.
    fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(
            &self
                .write_history()
                .iter()
                .flat_map(RewriteFacadeWriteEvent::shared_projections)
                .collect::<Vec<_>>(),
        )
    }

    /// Bootstraps multiple vertex refs and initial edges.
    fn bootstrap_vertex_refs_and_edges(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary>;

    /// Inserts one logical edge.
    fn insert_edge_pair_with_local_rebalance(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
    ) -> Result<GraphInsertWriteSummary, WritebackError>;

    /// Replaces one logical edge.
    fn replace_edge_pair(
        &mut self,
        spec: EdgeReplaceSpec,
    ) -> Result<RewriteReplaceEdgeSummary, WritebackError>;

    /// Tombstones one logical edge.
    fn tombstone_edge_pair(
        &mut self,
        spec: EdgeTombstoneSpec,
    ) -> Result<RewriteGraphMutationWriteSummary<GraphMutationPath>, WritebackError>;

    /// Flushes dirty state.
    fn flush_dirty(&mut self) -> RewriteGraphPmaResult<RewriteRefreshedVertices>;
}

/// Thin trait boundary for the rewrite-facing graph-pma facade.
///
/// This is intentionally small and non-object-safe. The goal is simply to let
/// upper layers depend on a stable facade-shaped contract while the concrete
/// rewrite implementation keeps evolving.
pub trait RewriteGraphStore {
    /// Stable-memory type backing this store (same as the bound handle passed to write/hydrate paths).
    type Mem: Memory;

    /// Returns the most recent facade-level write event observed through this store.
    fn last_write_event(&self) -> Option<&RewriteFacadeWriteEvent>;

    /// Returns recent facade-level write events in observation order.
    fn write_history(&self) -> &[RewriteFacadeWriteEvent];

    /// Returns recent facade-level write events projected into shared diagnostics history.
    fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(
            &self
                .write_history()
                .iter()
                .flat_map(RewriteFacadeWriteEvent::shared_projections)
                .collect::<Vec<_>>(),
        )
    }

    /// Returns the most recent facade-level write event projected into one diagnostics line.
    fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(
            &self
                .write_history()
                .iter()
                .flat_map(RewriteFacadeWriteEvent::shared_projections)
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

    /// Returns immutable access to the stable node property map.
    fn node_property_store(&self) -> &GraphPropertyStableMap<Self::Mem>;

    /// Returns mutable access to the stable node property map.
    fn node_property_store_mut(&mut self) -> &mut GraphPropertyStableMap<Self::Mem>;

    /// Returns immutable access to the stable edge property map.
    fn edge_property_store(&self) -> &GraphPropertyStableMap<Self::Mem>;

    /// Returns mutable access to the stable edge property map.
    fn edge_property_store_mut(&mut self) -> &mut GraphPropertyStableMap<Self::Mem>;

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
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError>;

    /// Like [`Self::remove_node_property_value`], plus a structured property-index mutation summary.
    fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError>;

    /// Like [`Self::set_edge_property_value`], plus a structured property-index mutation summary.
    fn set_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError>;

    /// Like [`Self::remove_edge_property_value`], plus a structured property-index mutation summary.
    fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError>;

    /// Appends or overwrites one node property, then flushes dirty state.
    fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary>;

    /// Appends one node-property tombstone, then flushes dirty state.
    fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary>;

    /// Appends or overwrites one edge property, then flushes dirty state.
    fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary>;

    /// Appends one edge-property tombstone, then flushes dirty state.
    fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary>;

    /// Rebuilds the canonical logical-locator sidecar from externally supplied forward-side ids.
    fn try_rebuild_logical_locator_sidecar(
        &mut self,
        forward_vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> RewriteGraphPmaResult<()>;

    /// Writes the full rewrite runtime state back to stable memory.
    fn try_write_all_to_stable_memory(&mut self, memory: &impl Memory)
    -> RewriteGraphPmaResult<()>;

    /// Refreshes dirty state and writes it back.
    fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<(Vec<usize>, Vec<usize>)>;

    /// Appends one empty vertex slot pair to both surfaces.
    fn append_empty_vertex_pair(&mut self) -> RewriteGraphPmaResult<(usize, usize)>;

    /// Appends `count` empty vertex slot pairs to both surfaces.
    fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> RewriteGraphPmaResult<Vec<(usize, usize)>>;

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
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary>;

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
    ) -> Result<RewriteReplaceEdgeSummary, WritebackError>;

    /// Tombstones one logical edge and writes back dirty state.
    fn tombstone_edge_pair_and_write(
        &mut self,
        spec: EdgeTombstoneSpec,
        memory: &impl Memory,
    ) -> Result<RewriteGraphMutationWriteSummary<GraphMutationPath>, WritebackError>;
}

const FACADE_WRITE_HISTORY_LIMIT: usize = 16;

impl<M: Memory> RewriteGraphPma<M> {
    fn record_write_event(&mut self, event: RewriteFacadeWriteEvent) {
        self.last_write_event = Some(event.clone());
        self.write_history.push(event);
        if self.write_history.len() > FACADE_WRITE_HISTORY_LIMIT {
            self.write_history.remove(0);
        }
    }

    /// Returns the most recent facade-level write event.
    pub fn last_write_event(&self) -> Option<&RewriteFacadeWriteEvent> {
        self.last_write_event.as_ref()
    }

    /// Returns recent facade-level write events in observation order.
    pub fn write_history(&self) -> &[RewriteFacadeWriteEvent] {
        &self.write_history
    }

    /// Returns the recent façade write history projected onto the shared event vocabulary.
    pub fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection> {
        self.write_history
            .iter()
            .flat_map(RewriteFacadeWriteEvent::shared_projections)
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
    pub fn maintenance_queue_projection(&self) -> Vec<RewriteMaintenanceQueueItemProjection> {
        self.graph
            .maintenance_queue()
            .iter()
            .copied()
            .map(RewriteMaintenanceQueueItemProjection::from_work_item)
            .collect()
    }

    /// Returns the retained maintenance queue formatted as diagnostics lines.
    pub fn formatted_maintenance_queue(&self) -> Vec<String> {
        format_maintenance_queue(&self.maintenance_queue_projection())
    }

    /// Reads the persisted maintenance queue directly from stable memory.
    pub fn try_read_maintenance_queue_from_stable_memory(
        &self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<Vec<GraphMaintenanceWorkItem>> {
        Self::load_maintenance_queue_from_stable_memory(&self.manager.borrow(), memory)
    }

    /// Reads the persisted maintenance queue directly from stable memory as structured projections.
    pub fn try_read_maintenance_queue_projection_from_stable_memory(
        &self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<Vec<RewriteMaintenanceQueueItemProjection>> {
        Ok(self
            .try_read_maintenance_queue_from_stable_memory(memory)?
            .into_iter()
            .map(RewriteMaintenanceQueueItemProjection::from_work_item)
            .collect())
    }

    /// Reads the persisted maintenance queue directly from stable memory as formatted diagnostics lines.
    pub fn try_format_maintenance_queue_from_stable_memory(
        &self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<Vec<String>> {
        Ok(format_maintenance_queue(
            &self.try_read_maintenance_queue_projection_from_stable_memory(memory)?,
        ))
    }

    /// Reads metadata for the persisted maintenance queue directly from stable memory.
    pub fn try_read_maintenance_queue_storage_projection_from_stable_memory(
        &self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteMaintenanceQueueStorageProjection> {
        let Some(region) = self
            .manager
            .borrow()
            .layout
            .region(RegionKind::MaintenanceQueue)
        else {
            return Ok(RewriteMaintenanceQueueStorageProjection {
                logical_len_bytes: 0,
                queue_len: 0,
                legacy_format: false,
                format_version: None,
                stored_checksum: None,
                computed_checksum: None,
                checksum_valid: None,
            });
        };
        let logical_len = usize::try_from(region.logical_len_bytes).map_err(|_| {
            RewriteGraphPmaError::Hydration(HydrationError::RegionTooLarge(
                RegionKind::MaintenanceQueue,
                region.logical_len_bytes,
            ))
        })?;
        if logical_len == 0 {
            return Ok(RewriteMaintenanceQueueStorageProjection {
                logical_len_bytes: region.logical_len_bytes,
                queue_len: 0,
                legacy_format: false,
                format_version: None,
                stored_checksum: None,
                computed_checksum: None,
                checksum_valid: None,
            });
        }
        let extent = self
            .manager
            .borrow()
            .region_extent(RegionKind::MaintenanceQueue)
            .ok_or(RewriteGraphPmaError::Hydration(
                HydrationError::MissingExtentRegion(RegionKind::MaintenanceQueue),
            ))?;
        if logical_len > usize::try_from(extent.len_bytes).unwrap_or(usize::MAX) {
            return Err(RewriteGraphPmaError::Hydration(
                HydrationError::LogicalLengthExceedsExtent {
                    kind: RegionKind::MaintenanceQueue,
                    logical_len_bytes: region.logical_len_bytes,
                    extent_len_bytes: extent.len_bytes,
                },
            ));
        }
        let mut bytes = vec![0u8; logical_len];
        memory.read(extent.addr.0, &mut bytes);
        if bytes.len() < Self::LEGACY_SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN {
            return Err(RewriteGraphPmaError::Hydration(
                HydrationError::InvalidLength {
                    kind: RegionKind::MaintenanceQueue,
                    expected_multiple: Self::LEGACY_SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN,
                    actual: bytes.len(),
                },
            ));
        }

        if bytes.len() >= Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN
            && bytes[..4] == Self::SERIALIZED_MAINTENANCE_QUEUE_MAGIC
        {
            let version = u32::from_le_bytes(bytes[4..8].try_into().expect("queue version"));
            if version != Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION {
                return Err(RewriteGraphPmaError::Hydration(
                    HydrationError::UnsupportedFormatVersion {
                        kind: RegionKind::MaintenanceQueue,
                        expected: Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION,
                        actual: version,
                    },
                ));
            }
            let queue_len =
                u64::from_le_bytes(bytes[8..16].try_into().expect("queue count")) as usize;
            let stored_checksum =
                u64::from_le_bytes(bytes[16..24].try_into().expect("queue checksum"));
            let body = &bytes[Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN..];
            let computed_checksum = Self::maintenance_queue_checksum(body);
            return Ok(RewriteMaintenanceQueueStorageProjection {
                logical_len_bytes: region.logical_len_bytes,
                queue_len,
                legacy_format: false,
                format_version: Some(version),
                stored_checksum: Some(stored_checksum),
                computed_checksum: Some(computed_checksum),
                checksum_valid: Some(stored_checksum == computed_checksum),
            });
        }

        Ok(RewriteMaintenanceQueueStorageProjection {
            logical_len_bytes: region.logical_len_bytes,
            queue_len: u64::from_le_bytes(
                bytes[..Self::LEGACY_SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN]
                    .try_into()
                    .expect("legacy queue len"),
            ) as usize,
            legacy_format: true,
            format_version: None,
            stored_checksum: None,
            computed_checksum: None,
            checksum_valid: None,
        })
    }

    /// Reads metadata for the persisted maintenance queue directly from stable memory as one diagnostics line.
    pub fn try_format_maintenance_queue_storage_from_stable_memory(
        &self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<String> {
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
        format_maintenance_queue_storage(&RewriteMaintenanceQueueStorageProjection {
            logical_len_bytes: Self::maintenance_queue_serialized_len(queue.len())
                .expect("maintenance queue serialized len should fit"),
            queue_len: queue.len(),
            legacy_format: false,
            format_version: Some(Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION),
            stored_checksum: Some(stored_checksum),
            computed_checksum: Some(stored_checksum),
            checksum_valid: Some(true),
        })
    }
}

impl<M: Memory> RewriteGraphPma<M> {
    const SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN: usize = 24;
    const SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN: usize = 56;
    const MAINTENANCE_QUEUE_LAST_EPOCH_NONE: u64 = u64::MAX;
    const SERIALIZED_MAINTENANCE_QUEUE_MAGIC: [u8; 4] = *b"MGQ1";
    const SERIALIZED_MAINTENANCE_QUEUE_VERSION: u32 = 1;
    const LEGACY_SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN: usize = 8;

    fn maintenance_queue_serialized_len(queue_len: usize) -> RewriteGraphPmaResult<u64> {
        let item_bytes = queue_len
            .checked_mul(Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN)
            .and_then(|n| n.checked_add(Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN))
            .ok_or({
                RewriteGraphPmaError::Hydration(HydrationError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    queue_len as u64,
                ))
            })?;
        u64::try_from(item_bytes).map_err(|_| {
            RewriteGraphPmaError::Hydration(HydrationError::RegionTooLarge(
                RegionKind::MaintenanceQueue,
                queue_len as u64,
            ))
        })
    }

    fn encode_maintenance_queue(
        queue: &[GraphMaintenanceWorkItem],
    ) -> RewriteGraphPmaResult<Vec<u8>> {
        let count = u64::try_from(queue.len()).map_err(|_| {
            RewriteGraphPmaError::Hydration(HydrationError::RegionTooLarge(
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

    fn decode_maintenance_queue(
        bytes: &[u8],
    ) -> RewriteGraphPmaResult<Vec<GraphMaintenanceWorkItem>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        if bytes.len() < Self::LEGACY_SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN {
            return Err(RewriteGraphPmaError::Hydration(
                HydrationError::InvalidLength {
                    kind: RegionKind::MaintenanceQueue,
                    expected_multiple: Self::LEGACY_SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN,
                    actual: bytes.len(),
                },
            ));
        }
        let (count, checksum, body) = if bytes.len()
            >= Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN
            && bytes[..4] == Self::SERIALIZED_MAINTENANCE_QUEUE_MAGIC
        {
            let version = u32::from_le_bytes(bytes[4..8].try_into().expect("queue version"));
            if version != Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION {
                return Err(RewriteGraphPmaError::Hydration(
                    HydrationError::UnsupportedFormatVersion {
                        kind: RegionKind::MaintenanceQueue,
                        expected: Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION,
                        actual: version,
                    },
                ));
            }
            (
                u64::from_le_bytes(bytes[8..16].try_into().expect("queue item count")) as usize,
                u64::from_le_bytes(bytes[16..24].try_into().expect("queue checksum")),
                &bytes[Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN..],
            )
        } else {
            (
                u64::from_le_bytes(
                    bytes[..Self::LEGACY_SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN]
                        .try_into()
                        .expect("legacy queue header len"),
                ) as usize,
                0,
                &bytes[Self::LEGACY_SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN..],
            )
        };
        let expected = count
            .checked_mul(Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN)
            .ok_or({
                RewriteGraphPmaError::Hydration(HydrationError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    body.len() as u64,
                ))
            })?;
        if body.len() != expected {
            return Err(RewriteGraphPmaError::Hydration(
                HydrationError::InvalidLength {
                    kind: RegionKind::MaintenanceQueue,
                    expected_multiple: Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN,
                    actual: body.len(),
                },
            ));
        }
        if bytes.len() >= Self::SERIALIZED_MAINTENANCE_QUEUE_HEADER_LEN
            && bytes[..4] == Self::SERIALIZED_MAINTENANCE_QUEUE_MAGIC
        {
            let actual_checksum = Self::maintenance_queue_checksum(body);
            if checksum != actual_checksum {
                return Err(RewriteGraphPmaError::Hydration(
                    HydrationError::ChecksumMismatch {
                        kind: RegionKind::MaintenanceQueue,
                        expected: checksum,
                        actual: actual_checksum,
                    },
                ));
            }
        }
        let mut queue = Vec::with_capacity(count);
        for chunk in body.chunks_exact(Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN) {
            let vertex =
                NodeId::try_from(u64::from_le_bytes(chunk[0..8].try_into().expect("vertex")))
                    .map_err(|_| {
                        RewriteGraphPmaError::Hydration(HydrationError::InvalidLength {
                            kind: RegionKind::MaintenanceQueue,
                            expected_multiple: Self::SERIALIZED_MAINTENANCE_QUEUE_ITEM_LEN,
                            actual: body.len(),
                        })
                    })?;
            let anchor_ordinal =
                usize::try_from(u64::from_le_bytes(chunk[8..16].try_into().expect("anchor")))
                    .map_err(|_| {
                        RewriteGraphPmaError::Hydration(HydrationError::RegionTooLarge(
                            RegionKind::MaintenanceQueue,
                            u64::MAX,
                        ))
                    })?;
            let window_start_ordinal = usize::try_from(u64::from_le_bytes(
                chunk[16..24].try_into().expect("window start"),
            ))
            .map_err(|_| {
                RewriteGraphPmaError::Hydration(HydrationError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    u64::MAX,
                ))
            })?;
            let window_end_ordinal_exclusive = usize::try_from(u64::from_le_bytes(
                chunk[24..32].try_into().expect("window end"),
            ))
            .map_err(|_| {
                RewriteGraphPmaError::Hydration(HydrationError::RegionTooLarge(
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

    fn ensure_maintenance_queue_region(manager: &mut RegionManager) -> Result<(), WritebackError> {
        if manager
            .layout
            .region(RegionKind::MaintenanceQueue)
            .is_none()
        {
            manager.define_extent_region(
                RegionKind::MaintenanceQueue,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    0,
                    WasmPages::new(1),
                    WasmPages::new(1),
                ),
            );
        }
        Ok(())
    }

    fn ensure_maintenance_queue_capacity(
        manager: &mut RegionManager,
        required_bytes: usize,
    ) -> Result<(), WritebackError> {
        Self::ensure_maintenance_queue_region(manager)?;
        let extent = manager.region_extent(RegionKind::MaintenanceQueue).ok_or(
            WritebackError::MissingExtentRegion(RegionKind::MaintenanceQueue),
        )?;
        let required_bytes = required_bytes as u64;
        if required_bytes <= extent.len_bytes {
            return Ok(());
        }
        let shortage = required_bytes.saturating_sub(extent.len_bytes);
        let additional_pages = shortage.div_ceil(crate::low_level::WASM_PAGE_SIZE);
        let request = ExtentGrowthRequest::new(WasmPages::new(additional_pages));
        let policy =
            ExtentGrowthPolicy::new(WasmPages::new(additional_pages.max(1)), WasmPages::new(1));
        if let Some(decision) =
            manager.plan_extent_growth(RegionKind::MaintenanceQueue, request, policy)
        {
            manager
                .apply_extent_growth(RegionKind::MaintenanceQueue, request, policy, decision)
                .ok_or(WritebackError::MissingExtentRegion(
                    RegionKind::MaintenanceQueue,
                ))?;
        }
        Ok(())
    }

    fn write_maintenance_queue_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> Result<u64, WritebackError> {
        let bytes =
            Self::encode_maintenance_queue(self.graph.maintenance_queue()).map_err(|_| {
                WritebackError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    self.graph.maintenance_queue().len() as u64,
                )
            })?;
        {
            let mut mgr = self.manager.borrow_mut();
            Self::ensure_maintenance_queue_capacity(&mut mgr, bytes.len())?;
            mgr.set_region_logical_len(RegionKind::MaintenanceQueue, bytes.len() as u64)
                .ok_or(WritebackError::MissingRegionDefinition(
                    RegionKind::MaintenanceQueue,
                ))?;
        }
        let extent_base = self
            .manager
            .borrow()
            .region_extent(RegionKind::MaintenanceQueue)
            .ok_or(WritebackError::MissingExtentRegion(
                RegionKind::MaintenanceQueue,
            ))?
            .addr
            .0;
        let last_byte_exclusive = extent_base + bytes.len() as u64;
        let current_bytes = memory.size() * crate::low_level::WASM_PAGE_SIZE;
        if last_byte_exclusive > current_bytes {
            let additional_pages =
                (last_byte_exclusive - current_bytes).div_ceil(crate::low_level::WASM_PAGE_SIZE);
            if memory.grow(additional_pages) < 0 {
                return Err(WritebackError::RegionTooLarge(
                    RegionKind::MaintenanceQueue,
                    last_byte_exclusive,
                ));
            }
        }
        if !bytes.is_empty() {
            memory.write(extent_base, &bytes);
        }
        Ok(bytes.len() as u64)
    }

    fn load_maintenance_queue_from_stable_memory(
        manager: &RegionManager,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<Vec<GraphMaintenanceWorkItem>> {
        let Some(region) = manager.layout.region(RegionKind::MaintenanceQueue) else {
            return Ok(Vec::new());
        };
        let logical_len = usize::try_from(region.logical_len_bytes).map_err(|_| {
            RewriteGraphPmaError::Hydration(HydrationError::RegionTooLarge(
                RegionKind::MaintenanceQueue,
                region.logical_len_bytes,
            ))
        })?;
        if logical_len == 0 {
            return Ok(Vec::new());
        }
        let extent = manager.region_extent(RegionKind::MaintenanceQueue).ok_or(
            RewriteGraphPmaError::Hydration(HydrationError::MissingExtentRegion(
                RegionKind::MaintenanceQueue,
            )),
        )?;
        if logical_len > usize::try_from(extent.len_bytes).unwrap_or(usize::MAX) {
            return Err(RewriteGraphPmaError::Hydration(
                HydrationError::LogicalLengthExceedsExtent {
                    kind: RegionKind::MaintenanceQueue,
                    logical_len_bytes: region.logical_len_bytes,
                    extent_len_bytes: extent.len_bytes,
                },
            ));
        }
        let mut bytes = vec![0u8; logical_len];
        if logical_len > 0 {
            memory.read(extent.addr.0, &mut bytes);
        }
        Self::decode_maintenance_queue(&bytes)
    }

    fn maintenance_queue_storage_snapshot_from_projection(
        projection: RewriteMaintenanceQueueStorageProjection,
    ) -> crate::low_level::GraphMaintenanceQueueStorageSnapshot {
        crate::low_level::GraphMaintenanceQueueStorageSnapshot {
            logical_len_bytes: projection.logical_len_bytes,
            queue_len: projection.queue_len,
            legacy_format: projection.legacy_format,
            format_version: projection.format_version,
            checksum_valid: projection.checksum_valid,
        }
    }

    /// Bundles an existing region manager and graph runtime into one facade.
    pub fn new(
        manager: Rc<RefCell<RegionManager>>,
        memory: Rc<RefCell<M>>,
        graph: GraphRuntime,
    ) -> Self {
        let btree_payload = Rc::new(RefCell::new(0u64));
        let property_equality_map = empty_property_equality_inplace_map(
            Rc::clone(&manager),
            Rc::clone(&memory),
            Rc::clone(&btree_payload),
        );
        let node_pl = Rc::new(RefCell::new(0u64));
        let edge_pl = Rc::new(RefCell::new(0u64));
        let node_property_store = empty_graph_property_stable_map(
            Rc::clone(&manager),
            Rc::clone(&memory),
            Rc::clone(&node_pl),
            RegionKind::NodePropertyStore,
        );
        let edge_property_store = empty_graph_property_stable_map(
            Rc::clone(&manager),
            Rc::clone(&memory),
            Rc::clone(&edge_pl),
            RegionKind::EdgePropertyStore,
        );
        Self {
            manager,
            memory,
            graph,
            node_property_store,
            edge_property_store,
            node_property_btree_payload: node_pl,
            edge_property_btree_payload: edge_pl,
            node_property_index: PropertyIndex::new(64),
            edge_property_index: PropertyIndex::new(64),
            property_equality_map,
            property_index_btree_payload: btree_payload,
            property_index_dirty: false,
            node_property_store_dirty: false,
            edge_property_store_dirty: false,
            last_write_event: None,
            write_history: Vec::new(),
            production_metrics: RewriteProductionMetrics::default(),
        }
    }

    /// Assembles a facade after hydration **without** re-initializing empty property btrees on disk.
    ///
    /// [`Self::new`] calls `StableBTreeMap::init` with payload length zero for each property region,
    /// which issues `BTreeMap::new` and overwrites stable memory. That must not run after
    /// `load_graph_property_stable_map_from_stable_memory` has read the live PSB1 image.
    fn assembled_after_property_load(
        manager: Rc<RefCell<RegionManager>>,
        memory: Rc<RefCell<M>>,
        graph: GraphRuntime,
        node_property_store: GraphPropertyStableMap<M>,
        edge_property_store: GraphPropertyStableMap<M>,
        node_property_btree_payload: Rc<RefCell<u64>>,
        edge_property_btree_payload: Rc<RefCell<u64>>,
        node_property_index: PropertyIndex,
        edge_property_index: PropertyIndex,
        property_equality_map: PropertyEqualityInplaceMap<M>,
        property_index_btree_payload: Rc<RefCell<u64>>,
        property_index_dirty: bool,
    ) -> Self {
        Self {
            manager,
            memory,
            graph,
            node_property_store,
            edge_property_store,
            node_property_btree_payload,
            edge_property_btree_payload,
            node_property_index,
            edge_property_index,
            property_equality_map,
            property_index_btree_payload,
            property_index_dirty,
            node_property_store_dirty: false,
            edge_property_store_dirty: false,
            last_write_event: None,
            write_history: Vec::new(),
            production_metrics: RewriteProductionMetrics::default(),
        }
    }

    /// Bootstraps one empty rewrite graph with the default bucket granularity.
    pub fn bootstrap_empty(memory: M) -> RewriteGraphPmaResult<Self> {
        Self::bootstrap_empty_with_bucket_size(BucketSizeInPages::DEFAULT, memory)
    }

    /// Bootstraps one empty rewrite graph with an explicit bucket granularity.
    pub fn bootstrap_empty_with_bucket_size(
        bucket_size_in_pages: BucketSizeInPages,
        memory: M,
    ) -> RewriteGraphPmaResult<Self> {
        Self::bootstrap_empty_with_bucket_size_using_memory_rc(
            bucket_size_in_pages,
            Rc::new(RefCell::new(memory)),
        )
    }

    /// Like [`Self::bootstrap_empty_with_bucket_size`], but reuses one shared [`Rc<RefCell<M>>`].
    pub fn bootstrap_empty_with_bucket_size_using_memory_rc(
        bucket_size_in_pages: BucketSizeInPages,
        mem_rc: Rc<RefCell<M>>,
    ) -> RewriteGraphPmaResult<Self> {
        let mut manager = RegionManager::with_bucket_size(bucket_size_in_pages);
        Self::define_empty_surface_regions(&mut manager, crate::low_level::SurfaceKind::Forward);
        Self::define_empty_surface_regions(&mut manager, crate::low_level::SurfaceKind::Reverse);
        Self::define_empty_property_regions(&mut manager);

        let mgr_rc = Rc::new(RefCell::new(manager));
        let forward = ForwardSurfaceRuntime::without_overflow(
            forward_surface_from_layout(&mgr_rc.borrow().layout)?,
            Vec::new(),
        );
        let reverse = ReverseSurfaceRuntime::without_overflow(
            reverse_surface_from_layout(&mgr_rc.borrow().layout)?,
            Vec::new(),
        );
        let mut facade = RewriteGraphPma::new(
            Rc::clone(&mgr_rc),
            Rc::clone(&mem_rc),
            GraphRuntime::new_with_empty_sidecars(forward, reverse),
        );
        facade.try_write_all_to_stable_memory(&*mem_rc.borrow())?;
        Ok(facade)
    }

    /// Creates one facade from already-hydrated directional runtimes.
    pub fn from_hydrated_runtimes(
        manager: Rc<RefCell<RegionManager>>,
        memory: Rc<RefCell<M>>,
        runtimes: HydratedSurfaceRuntimes,
    ) -> Self {
        let mut graph = GraphRuntime::new_with_empty_sidecars(runtimes.forward, runtimes.reverse);
        let _ = graph.sync_base_segment_capacities_from_manager(&manager.borrow());
        Self::new(manager, memory, graph)
    }

    /// Creates one facade from hydrated runtimes and an explicit insert policy.
    pub fn from_hydrated_runtimes_with_insert_policy(
        manager: Rc<RefCell<RegionManager>>,
        memory: Rc<RefCell<M>>,
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

    /// Hydrates forward/reverse runtimes from stable memory and builds a facade.
    ///
    /// The locator sidecar starts empty. Callers that already know the
    /// canonical forward-side semantic edge ids can repopulate it after
    /// hydration using the lower-level sidecar helpers.
    ///
    /// Property indices are loaded through [`PropertyIndexStorageImage::try_from_sectioned_parts`],
    /// which normalizes an **empty on-disk logical snapshot** against non-empty node stores so
    /// in-memory `node_property_index` / `edge_property_index` match persisted pages.
    pub fn hydrate_from_stable_memory(
        manager: RegionManager,
        memory: M,
    ) -> RewriteGraphPmaResult<Self> {
        let mgr_rc = Rc::new(RefCell::new(manager));
        let mem_rc = Rc::new(RefCell::new(memory));
        let runtimes =
            hydrate_surface_runtimes_from_stable_memory(&mgr_rc.borrow(), &*mem_rc.borrow())?;
        let (node_property_store, node_pl, node_mig) =
            load_graph_property_stable_map_from_stable_memory(
                Rc::clone(&mgr_rc),
                Rc::clone(&mem_rc),
                RegionKind::NodePropertyStore,
            )?;
        let (edge_property_store, edge_pl, edge_mig) =
            load_graph_property_stable_map_from_stable_memory(
                Rc::clone(&mgr_rc),
                Rc::clone(&mem_rc),
                RegionKind::EdgePropertyStore,
            )?;

        let mut graph = GraphRuntime::new_with_empty_sidecars(runtimes.forward, runtimes.reverse);
        let _ = graph.sync_base_segment_capacities_from_manager(&mgr_rc.borrow());

        let pidx_header =
            read_pidx_v3_header_from_stable_memory(&mgr_rc.borrow(), &*mem_rc.borrow())?;
        let (
            property_equality_map,
            property_index_btree_payload,
            node_property_index,
            edge_property_index,
            property_index_dirty,
        ) = if let Some(header) = pidx_header {
            let virt = ensure_pidx_v3_btree_subregion_for_hydrate(
                &mut mgr_rc.borrow_mut(),
                &*mem_rc.borrow(),
                &header,
            )?;
            let btree_rc = Rc::new(RefCell::new(virt));
            let property_equality_map = hydrate_property_equality_inplace_map(
                Rc::clone(&mgr_rc),
                Rc::clone(&mem_rc),
                Rc::clone(&btree_rc),
            );
            let snap = snapshot_from_equality_any_memory(&property_equality_map, 64);
            (
                property_equality_map,
                btree_rc,
                snap.node_index,
                snap.edge_index,
                false,
            )
        } else {
            let btree_rc = Rc::new(RefCell::new(0u64));
            let property_equality_map = empty_property_equality_inplace_map(
                Rc::clone(&mgr_rc),
                Rc::clone(&mem_rc),
                Rc::clone(&btree_rc),
            );
            (
                property_equality_map,
                btree_rc,
                PropertyIndex::new(64),
                PropertyIndex::new(64),
                false,
            )
        };

        let mut facade = Self::assembled_after_property_load(
            mgr_rc,
            mem_rc,
            graph,
            node_property_store,
            edge_property_store,
            node_pl,
            edge_pl,
            node_property_index,
            edge_property_index,
            property_equality_map,
            property_index_btree_payload,
            property_index_dirty,
        );

        if facade.node_property_index.header.entry_count == 0
            && facade.edge_property_index.header.entry_count == 0
            && (!facade.node_property_store.is_empty() || !facade.edge_property_store.is_empty())
        {
            facade.rebuild_property_indices()?;
        }
        let maintenance_queue = Self::load_maintenance_queue_from_stable_memory(
            &facade.manager.borrow(),
            &*facade.memory.borrow(),
        )?;
        facade.graph.replace_maintenance_queue(maintenance_queue);
        facade.node_property_store_dirty = node_mig;
        facade.edge_property_store_dirty = edge_mig;
        Ok(facade)
    }

    /// Hydrates forward/reverse runtimes from stable memory with an explicit insert policy.
    pub fn hydrate_from_stable_memory_with_insert_policy(
        manager: RegionManager,
        memory: M,
        insert_policy: GraphInsertPolicy,
    ) -> RewriteGraphPmaResult<Self> {
        let mgr_rc = Rc::new(RefCell::new(manager));
        let mem_rc = Rc::new(RefCell::new(memory));
        let runtimes =
            hydrate_surface_runtimes_from_stable_memory(&mgr_rc.borrow(), &*mem_rc.borrow())?;
        let (node_property_store, node_pl, node_mig) =
            load_graph_property_stable_map_from_stable_memory(
                Rc::clone(&mgr_rc),
                Rc::clone(&mem_rc),
                RegionKind::NodePropertyStore,
            )?;
        let (edge_property_store, edge_pl, edge_mig) =
            load_graph_property_stable_map_from_stable_memory(
                Rc::clone(&mgr_rc),
                Rc::clone(&mem_rc),
                RegionKind::EdgePropertyStore,
            )?;

        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            runtimes.forward,
            runtimes.reverse,
            insert_policy,
        );
        let _ = graph.sync_base_segment_capacities_from_manager(&mgr_rc.borrow());

        let pidx_header =
            read_pidx_v3_header_from_stable_memory(&mgr_rc.borrow(), &*mem_rc.borrow())?;
        let (
            property_equality_map,
            property_index_btree_payload,
            node_property_index,
            edge_property_index,
            property_index_dirty,
        ) = if let Some(header) = pidx_header {
            let virt = ensure_pidx_v3_btree_subregion_for_hydrate(
                &mut mgr_rc.borrow_mut(),
                &*mem_rc.borrow(),
                &header,
            )?;
            let btree_rc = Rc::new(RefCell::new(virt));
            let property_equality_map = hydrate_property_equality_inplace_map(
                Rc::clone(&mgr_rc),
                Rc::clone(&mem_rc),
                Rc::clone(&btree_rc),
            );
            let snap = snapshot_from_equality_any_memory(&property_equality_map, 64);
            (
                property_equality_map,
                btree_rc,
                snap.node_index,
                snap.edge_index,
                false,
            )
        } else {
            let btree_rc = Rc::new(RefCell::new(0u64));
            let property_equality_map = empty_property_equality_inplace_map(
                Rc::clone(&mgr_rc),
                Rc::clone(&mem_rc),
                Rc::clone(&btree_rc),
            );
            (
                property_equality_map,
                btree_rc,
                PropertyIndex::new(64),
                PropertyIndex::new(64),
                false,
            )
        };

        let mut facade = Self::assembled_after_property_load(
            mgr_rc,
            mem_rc,
            graph,
            node_property_store,
            edge_property_store,
            node_pl,
            edge_pl,
            node_property_index,
            edge_property_index,
            property_equality_map,
            property_index_btree_payload,
            property_index_dirty,
        );

        if facade.node_property_index.header.entry_count == 0
            && facade.edge_property_index.header.entry_count == 0
            && (!facade.node_property_store.is_empty() || !facade.edge_property_store.is_empty())
        {
            facade.rebuild_property_indices()?;
        }
        let maintenance_queue = Self::load_maintenance_queue_from_stable_memory(
            &facade.manager.borrow(),
            &*facade.memory.borrow(),
        )?;
        facade.graph.replace_maintenance_queue(maintenance_queue);
        facade.node_property_store_dirty = node_mig;
        facade.edge_property_store_dirty = edge_mig;
        Ok(facade)
    }

    /// Hydrates one rewrite facade from stable memory using the facade-level result type.
    pub fn try_hydrate_from_stable_memory(
        manager: RegionManager,
        memory: M,
    ) -> RewriteGraphPmaResult<Self> {
        Self::hydrate_from_stable_memory(manager, memory)
    }

    /// Hydrates one rewrite facade with an explicit insert policy using the facade-level result type.
    pub fn try_hydrate_from_stable_memory_with_insert_policy(
        manager: RegionManager,
        memory: M,
        insert_policy: GraphInsertPolicy,
    ) -> RewriteGraphPmaResult<Self> {
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
    ) -> RewriteGraphPmaResult<Self> {
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
    ) -> RewriteGraphPmaResult<Self> {
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

    /// Returns immutable access to the stable node property map.
    pub fn node_property_store(&self) -> &GraphPropertyStableMap<M> {
        &self.node_property_store
    }

    /// Returns mutable access to the stable node property map.
    pub fn node_property_store_mut(&mut self) -> &mut GraphPropertyStableMap<M> {
        &mut self.node_property_store
    }

    /// Returns immutable access to the stable edge property map.
    pub fn edge_property_store(&self) -> &GraphPropertyStableMap<M> {
        &self.edge_property_store
    }

    /// Returns mutable access to the stable edge property map.
    pub fn edge_property_store_mut(&mut self) -> &mut GraphPropertyStableMap<M> {
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
        let encoded_value = value
            .to_binary_bytes()
            .expect("Value must encode to binary bytes");
        Ok(scan_node_property_index_value_prefix_from_stable_memory(
            &self.manager.borrow(),
            memory,
            property,
            &encoded_value,
        )?
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
        Ok(scan_node_property_index_property_prefix_from_stable_memory(
            &self.manager.borrow(),
            memory,
            property,
        )?
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
        let encoded_value = value
            .to_binary_bytes()
            .expect("Value must encode to binary bytes");
        Ok(scan_edge_property_index_value_prefix_from_stable_memory(
            &self.manager.borrow(),
            memory,
            property,
            &encoded_value,
        )?
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
        Ok(scan_edge_property_index_property_prefix_from_stable_memory(
            &self.manager.borrow(),
            memory,
            property,
        )?
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
            self.graph.insert_policy,
            anchor_live_degree_after_rebalance,
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
            self.graph.insert_policy,
            anchor_live_degree_after_rebalance,
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
        let started = Instant::now();
        if !self.node_property_store_dirty
            && let Ok(ids) =
                self.try_scan_node_ids_by_property_eq_from_stable_memory(memory, property, value)
        {
            self.production_metrics
                .record_node_eq_scan_nanos(started.elapsed().as_nanos() as u64);
            return ids;
        }
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
        if !self.node_property_store_dirty
            && let Ok(ids) = self.try_scan_node_ids_by_property_from_stable_memory(memory, property)
        {
            return ids;
        }
        self.scan_node_ids_by_property(property)
    }

    /// Returns edge ids matching one equality predicate, preferring stable-memory direct scan when clean.
    pub fn scan_edge_ids_by_property_eq_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
        value: &Value,
    ) -> Vec<EdgeId> {
        let started = Instant::now();
        if !self.edge_property_store_dirty
            && let Ok(ids) =
                self.try_scan_edge_ids_by_property_eq_from_stable_memory(memory, property, value)
        {
            self.production_metrics
                .record_edge_eq_scan_nanos(started.elapsed().as_nanos() as u64);
            return ids;
        }
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
        if !self.edge_property_store_dirty
            && let Ok(ids) = self.try_scan_edge_ids_by_property_from_stable_memory(memory, property)
        {
            return ids;
        }
        self.scan_edge_ids_by_property(property)
    }

    /// Returns whether node-side property state has unflushed changes.
    pub const fn node_property_store_is_dirty(&self) -> bool {
        self.node_property_store_dirty
    }

    /// Returns whether edge-side property state has unflushed changes.
    pub const fn edge_property_store_is_dirty(&self) -> bool {
        self.edge_property_store_dirty
    }

    /// Returns a snapshot of production-facing property/index metrics.
    pub fn production_metrics_snapshot(&self) -> RewriteProductionMetricsSnapshot {
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
    ) -> RewriteGraphPmaResult<()> {
        self.rebuild_logical_locator_sidecar(forward_vertex_refs, forward_base_edge_ids_by_ordinal)
            .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RewriteAppendVertexWriteSummary, RewriteAppendVerticesWriteSummary,
        RewriteBootstrapEdgeProjection, RewriteBootstrapEdgeWriteSummary,
        RewriteBootstrapGraphProjection, RewriteBootstrapGraphWriteSummary,
        RewriteBootstrapVerticesProjection, RewriteEdgeLogicalLocatorMapping,
        RewriteEdgeWriteOperation, RewriteEnsureCapacityProjection, RewriteFacadeWriteEvent,
        RewriteGraphMutationWriteSummary, RewriteGraphPma, RewriteGraphPmaError,
        RewriteGraphPmaResult, RewriteGraphService, RewriteGraphStore, RewriteGraphStoreAdapter,
        RewriteInsertEdgeProjection, RewriteMaintenanceBatchProjection,
        RewriteMaintenanceCycleProjection, RewriteMaintenanceQueueAction,
        RewriteMaintenanceQueueStorageProjection, RewritePropertyIndexTouchedSections,
        RewriteRefreshedVertices, RewriteVertexOrdinalMapping, RewriteWriteEventProjection,
    };
    use crate::GraphInsertResult;
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
        PIDX_V3_MAGIC, PropertyIndexSnapshot, PropertyIndexStorageImage,
        hydrate_property_equality_map_from_serialized_bytes, read_property_index_region_magic,
        serialize_property_equality_btree, write_property_index_storage_image_to_stable_memory,
    };
    use crate::property_index::{PropertyIndexError, PropertyIndexNodeStoreMutationKind};
    use crate::property_store::{
        PropertyKey, PropertyStoreError, StoredPropertyValue,
        load_graph_property_stable_map_from_stable_memory,
        sync_graph_property_store_v1_header_to_stable_memory,
    };
    use crate::stable::{Memory, VecMemory};
    use gleaph_gql::Value;
    use gleaph_graph_kernel::NodeId;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::Ordering;

    type TestPma = RewriteGraphPma<VecMemory>;

    fn assert_projected_history(
        events: &[RewriteFacadeWriteEvent],
        expected: Vec<RewriteWriteEventProjection>,
    ) {
        assert_eq!(project_facade_write_history(events), expected);
    }

    fn define_surface_regions(manager: &mut RegionManager, prefix: crate::low_level::SurfaceKind) {
        RewriteGraphPma::<VecMemory>::define_empty_surface_regions(manager, prefix);
    }

    fn define_property_regions(manager: &mut RegionManager) {
        RewriteGraphPma::<VecMemory>::define_empty_property_regions(manager);
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
        let forward_edges = vec![EdgeEntry::new(
            NodeId::new([0, 0, 0, 0, 0, 2]),
            EdgeMeta::new(7, false),
        )];
        let reverse_vertices = vec![VertexEntry::new(
            EdgeIndex::new(0),
            1,
            crate::low_level::EMPTY_LOG_OFFSET,
        )];
        let reverse_edges = vec![EdgeEntry::new(
            NodeId::new([0, 0, 0, 0, 0, 1]),
            EdgeMeta::new(7, false),
        )];

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
        let facade = RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        assert_eq!(facade.graph.forward.0.vertices.len(), 1);
        assert_eq!(facade.graph.forward.0.base_entries.len(), 1);
        assert_eq!(facade.graph.reverse.0.vertices.len(), 1);
        assert_eq!(facade.graph.reverse.0.base_entries.len(), 1);
    }

    #[test]
    fn surface_write_after_property_insert_does_not_invalidate_node_property_btree() {
        let mem_rc = Rc::new(RefCell::new(VecMemory::default()));
        let mut facade = RewriteGraphPma::bootstrap_empty_with_bucket_size_using_memory_rc(
            BucketSizeInPages::new(1),
            Rc::clone(&mem_rc),
        )
        .unwrap();
        let node_id = NodeId::from(11u8);
        let _ = facade.node_property_store_mut().insert(
            PropertyKey::node(node_id, "profile"),
            StoredPropertyValue(Value::Text("y".repeat((WASM_PAGE_SIZE as usize) + 512))),
        );
        let runtimes = HydratedSurfaceRuntimes::new(
            facade.graph.forward.clone(),
            facade.graph.reverse.clone(),
        );
        write_surface_runtimes_to_stable_memory(
            &mut *facade.manager.borrow_mut(),
            &*mem_rc.borrow(),
            &runtimes,
        )
        .expect("surface write");
        sync_graph_property_store_v1_header_to_stable_memory(
            &mut *facade.manager.borrow_mut(),
            &*mem_rc.borrow(),
            RegionKind::NodePropertyStore,
            *facade.node_property_btree_payload.borrow(),
        )
        .expect("sync psb header");
        let _ = load_graph_property_stable_map_from_stable_memory(
            Rc::clone(&facade.manager),
            Rc::clone(&facade.memory),
            RegionKind::NodePropertyStore,
        )
        .expect("reload node property btree");
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
        let mem_rc = Rc::new(RefCell::new(VecMemory::default()));
        let facade = RewriteGraphPma::from_hydrated_runtimes(mgr_rc, mem_rc, runtimes);

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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
            .try_refresh_and_write_dirty_to_stable_memory(&*mem_rc.borrow())
            .expect("write dirty including queue");

        let hydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
            .try_refresh_and_write_dirty_to_stable_memory(&*mem_rc.borrow())
            .expect("persist graph state");
        facade
            .rebuild_maintenance_queue_at_epoch_and_write(
                &[src.into(), dst.into()],
                11,
                &*mem_rc.borrow(),
            )
            .expect("persist maintenance queue");

        let queue = facade
            .try_read_maintenance_queue_from_stable_memory(&*mem_rc.borrow())
            .expect("read queue bytes");
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].start_ordinal, 0);

        let projection = facade
            .try_read_maintenance_queue_projection_from_stable_memory(&*mem_rc.borrow())
            .expect("read queue projection");
        assert_eq!(projection.len(), 2);
        assert!(projection.iter().any(|item| item.vertex_ref == src.into()));

        let formatted = facade
            .try_format_maintenance_queue_from_stable_memory(&*mem_rc.borrow())
            .expect("format queue");
        assert_eq!(formatted.len(), 2);
        assert!(
            formatted
                .iter()
                .any(|line| line.contains("maintenance-queue vertex=83")
                    && line.contains("window=(0, 1)"))
        );

        let storage = facade
            .try_read_maintenance_queue_storage_projection_from_stable_memory(&*mem_rc.borrow())
            .expect("read queue storage projection");
        assert!(!storage.legacy_format);
        assert_eq!(
            storage.format_version,
            Some(TestPma::SERIALIZED_MAINTENANCE_QUEUE_VERSION)
        );
        assert_eq!(storage.queue_len, 2);
        assert_eq!(storage.checksum_valid, Some(true));
        assert!(storage.logical_len_bytes > 0);

        let formatted_storage = facade
            .try_format_maintenance_queue_storage_from_stable_memory(&*mem_rc.borrow())
            .expect("format storage metadata");
        assert!(formatted_storage.contains("maintenance-queue-storage"));
        assert!(formatted_storage.contains("legacy=false"));
        assert!(formatted_storage.contains("checksum="));
    }

    #[test]
    fn facade_rebuild_maintenance_queue_and_write_persists_queue_without_graph_writeback() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
            .try_refresh_and_write_dirty_to_stable_memory(&*mem_rc.borrow())
            .expect("persist graph state");

        assert_eq!(
            facade
                .rebuild_maintenance_queue_at_epoch_and_write(
                    &[src.into(), dst.into()],
                    11,
                    &*mem_rc.borrow(),
                )
                .expect("persist queue"),
            Some(2)
        );

        let hydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
        )
        .unwrap();
        assert_eq!(hydrated.maintenance_queue_projection().len(), 2);
    }

    #[test]
    fn facade_decodes_legacy_maintenance_queue_format() {
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

        let decoded = TestPma::decode_maintenance_queue(&bytes).expect("legacy decode");

        assert_eq!(decoded, vec![item]);
    }

    #[test]
    fn facade_rejects_unsupported_maintenance_queue_format_version() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&TestPma::SERIALIZED_MAINTENANCE_QUEUE_MAGIC);
        bytes.extend_from_slice(&99u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());

        match TestPma::decode_maintenance_queue(&bytes) {
            Err(RewriteGraphPmaError::Hydration(HydrationError::UnsupportedFormatVersion {
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
            Err(RewriteGraphPmaError::Hydration(HydrationError::ChecksumMismatch {
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
    fn facade_can_read_legacy_maintenance_queue_storage_projection() {
        let memory = VecMemory::default();
        let facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
        TestPma::ensure_maintenance_queue_capacity(&mut facade.manager.borrow_mut(), bytes.len())
            .expect("ensure queue capacity");
        facade
            .manager
            .borrow_mut()
            .set_region_logical_len(RegionKind::MaintenanceQueue, bytes.len() as u64)
            .expect("set queue logical len");
        let extent = facade
            .manager
            .borrow()
            .region_extent(RegionKind::MaintenanceQueue)
            .expect("queue extent");
        facade.memory.borrow().write(extent.addr.0, &bytes);

        let storage = facade
            .try_read_maintenance_queue_storage_projection_from_stable_memory(
                &*facade.memory.borrow(),
            )
            .expect("read legacy queue storage");

        assert!(storage.legacy_format);
        assert_eq!(storage.queue_len, 1);
        assert_eq!(storage.format_version, None);
        assert_eq!(storage.stored_checksum, None);
        assert_eq!(storage.checksum_valid, None);
    }

    #[test]
    fn facade_can_read_vertex_entry_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

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
        let facade = RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

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
        let facade = RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let forward = facade
            .try_read_forward_vertex_base_entries_from_stable_memory(&memory, 0)
            .expect("forward base-entry read should succeed");
        let reverse = facade
            .try_read_reverse_vertex_base_entries_from_stable_memory(&memory, 0)
            .expect("reverse base-entry read should succeed");

        assert_eq!(
            forward,
            Some(vec![EdgeEntry::new(
                NodeId::new([0, 0, 0, 0, 0, 2]),
                EdgeMeta::new(7, false)
            )])
        );
        assert_eq!(
            reverse,
            Some(vec![EdgeEntry::new(
                NodeId::new([0, 0, 0, 0, 0, 1]),
                EdgeMeta::new(7, false)
            )])
        );
    }

    #[test]
    fn facade_can_read_edge_entries_by_ref_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

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
            vec![EdgeEntry::new(
                NodeId::new([0, 0, 0, 0, 0, 2]),
                EdgeMeta::new(7, false)
            )]
        );
        assert_eq!(
            reverse,
            vec![EdgeEntry::new(
                NodeId::new([0, 0, 0, 0, 0, 1]),
                EdgeMeta::new(7, false)
            )]
        );
    }

    #[test]
    fn facade_can_read_base_edge_ref_and_entry_directly_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

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
            Some(EdgeEntry::new(
                NodeId::new([0, 0, 0, 0, 0, 2]),
                EdgeMeta::new(7, false)
            ))
        );
        assert_eq!(
            reverse_entry,
            Some(EdgeEntry::new(
                NodeId::new([0, 0, 0, 0, 0, 1]),
                EdgeMeta::new(7, false)
            ))
        );
    }

    #[test]
    fn facade_prefers_runtime_vertex_entry_when_vertex_table_is_dirty() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let src = NodeId::new([0, 0, 0, 0, 0, 1]);
        let dst = NodeId::new([0, 0, 0, 0, 0, 3]);
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
            .refresh_and_write_dirty_to_stable_memory(&*mem_rc.borrow())
            .unwrap();

        let rehydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
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
        let mut facade = RewriteGraphPma::bootstrap_empty_with_bucket_size(
            BucketSizeInPages::new(1),
            memory.clone(),
        )
        .expect("bootstrap");
        let node_id = NodeId::from(11u8);

        let _ = facade.node_property_store_mut().insert(
            PropertyKey::node(node_id, "profile"),
            StoredPropertyValue(Value::Text(
                "x".repeat((crate::low_level::WASM_PAGE_SIZE as usize) + 512),
            )),
        );
        let _ = facade.edge_property_store_mut().insert(
            PropertyKey::edge(77, "weight"),
            StoredPropertyValue(Value::Int64(9)),
        );
        facade.node_property_store_dirty = true;
        facade.edge_property_store_dirty = true;
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(&*mem_rc.borrow())
            .expect("write all");

        let rehydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
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
    fn facade_property_store_dirty_write_round_trips_and_clears_dirty_flags() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(21u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("u21".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(901, "weight", &Value::Int64(42))
            .expect("set edge property");
        assert!(facade.node_property_store_dirty);
        assert!(facade.edge_property_store_dirty);

        let mem_rc = Rc::clone(&facade.memory);
        let refreshed = facade
            .refresh_and_write_dirty_to_stable_memory(&*mem_rc.borrow())
            .expect("write dirty");
        assert_eq!(refreshed, (Vec::new(), Vec::new()));
        assert!(!facade.node_property_store_dirty);
        assert!(!facade.edge_property_store_dirty);

        let rehydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
            .refresh_and_write_dirty_to_stable_memory(&*mem_rc_flush.borrow())
            .expect("flush dirty");

        // rehydrate + query
        let mut rehydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
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

        // mutate again + flush + rehydrate again
        rehydrated
            .set_node_property_value(node_id, "uid", &Value::Text("u41b".into()))
            .expect("overwrite node property");
        rehydrated
            .remove_edge_property_value(edge_id, "weight")
            .expect("remove edge property");
        let mem_rc2 = Rc::clone(&rehydrated.memory);
        rehydrated
            .refresh_and_write_dirty_to_stable_memory(&*mem_rc2.borrow())
            .expect("flush dirty second time");

        let rehydrated2 = RewriteGraphPma::hydrate_from_stable_memory(
            rehydrated.manager.borrow().clone(),
            rehydrated.memory.borrow().clone(),
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
        // One test: avoid parallel tests racing on the injected atomics.
        let memory = VecMemory::default();

        //
        // Node property: injected index-bind error
        //
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(1u8);
        facade
            .set_node_property_value(node_id, "a", &Value::Text("hello".into()))
            .expect("seed node property");
        super::FAIL_NEXT_NODE_PROPERTY_INDEX_SYNC_TEST.store(true, Ordering::SeqCst);
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let edge_id = 77u64;
        facade
            .set_edge_property_value(edge_id, "kind", &Value::Text("follows".into()))
            .expect("seed edge property");
        super::FAIL_NEXT_EDGE_PROPERTY_INDEX_SYNC_TEST.store(true, Ordering::SeqCst);
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(32u8);

        let first = facade
            .set_node_property_value_with_summary(node_id, "uid", &Value::Text("alice".into()))
            .expect("set initial property with summary");
        assert_eq!(
            first.sections,
            RewritePropertyIndexTouchedSections {
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
            RewritePropertyIndexTouchedSections {
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
            RewritePropertyIndexTouchedSections {
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
            RewritePropertyIndexTouchedSections {
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");

        let set = facade
            .set_edge_property_value_with_summary(701, "weight", &Value::Int64(5))
            .expect("set edge property with summary");
        assert_eq!(
            set.sections,
            RewritePropertyIndexTouchedSections {
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
            RewritePropertyIndexTouchedSections {
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(33u8);

        let mem_rc = Rc::clone(&facade.memory);
        let mem_guard = mem_rc.borrow();
        let summary = facade
            .set_node_property_value_and_write(
                node_id,
                "uid",
                &Value::Text("carol".into()),
                &*mem_guard,
            )
            .expect("set property and write");
        assert_eq!(
            summary.flushed_sections,
            RewritePropertyIndexTouchedSections {
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
                    &*mem_guard,
                    "uid",
                    &Value::Text("carol".into()),
                )
                .contains(&node_id)
        );

        let rehydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
        )
        .unwrap();
        assert_eq!(
            rehydrated.scan_node_ids_by_property_eq("uid", &Value::Text("carol".into())),
            vec![node_id]
        );
        assert!(matches!(
            facade.last_write_event(),
            Some(RewriteFacadeWriteEvent::Property(_))
        ));
        let event_projection = facade
            .last_write_event()
            .and_then(RewriteFacadeWriteEvent::property_projection)
            .expect("property event projection");
        assert_eq!(summary.projection(), event_projection);
        assert!(matches!(
            facade.write_history(),
            [RewriteFacadeWriteEvent::Property(_)]
        ));
        assert_eq!(
            project_facade_write_event(facade.write_history().last().expect("last facade event")),
            vec![RewriteWriteEventProjection::Property(summary.projection())]
        );
    }

    #[test]
    fn facade_edge_property_mutation_write_summary_flushes_and_clears_dirty_state() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");

        let mem_rc = Rc::clone(&facade.memory);
        let mem_guard = mem_rc.borrow();
        let set = facade
            .set_edge_property_value_and_write(702, "weight", &Value::Int64(9), &*mem_guard)
            .expect("set edge property and write");
        assert_eq!(
            set.flushed_sections,
            RewritePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert!(!facade.edge_property_store_is_dirty());

        let remove = facade
            .remove_edge_property_value_and_write(702, "weight", &*mem_guard)
            .expect("remove edge property and write");
        assert_eq!(
            remove.flushed_sections,
            RewritePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert!(!facade.edge_property_store_is_dirty());
        assert!(
            facade
                .scan_edge_ids_by_property_eq_preferring_stable_memory(
                    &*mem_guard,
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
                RewriteFacadeWriteEvent::Property(_),
                RewriteFacadeWriteEvent::Property(_)
            ]
        ));
        assert_projected_history(
            facade.write_history(),
            vec![
                RewriteWriteEventProjection::Property(set.projection()),
                RewriteWriteEventProjection::Property(remove.projection()),
            ],
        );
        assert_eq!(
            project_facade_write_event(facade.write_history().last().expect("last facade event")),
            vec![RewriteWriteEventProjection::Property(remove.projection())]
        );
    }

    #[test]
    fn facade_records_edge_write_events_in_unified_history() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
                    label_id: 9,
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
                RewriteFacadeWriteEvent::BootstrapGraph(_),
                RewriteFacadeWriteEvent::ReplaceEdge(_),
                RewriteFacadeWriteEvent::DeleteEdge(_)
            ]
        ));
        assert!(matches!(
            facade.last_write_event(),
            Some(RewriteFacadeWriteEvent::DeleteEdge(_))
        ));
    }

    #[test]
    fn facade_property_index_snapshot_round_trips_through_stable_memory() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(41u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(777, "weight", &Value::Int64(5))
            .expect("set edge property");
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(&*mem_rc.borrow())
            .expect("write all");

        let rehydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
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
    fn facade_writes_pidx_v3_region_with_nonempty_btree() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(42u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(778, "weight", &Value::Int64(5))
            .expect("set edge property");
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(&*mem_rc.borrow())
            .expect("write all");

        assert_eq!(
            read_property_index_region_magic(&*facade.manager.borrow(), &*facade.memory.borrow())
                .expect("magic"),
            Some(PIDX_V3_MAGIC)
        );
        let image = crate::property_index::read_property_index_storage_image_from_stable_memory(
            &*facade.manager.borrow(),
            &*facade.memory.borrow(),
        )
        .expect("read storage image");
        assert!(image.equality_map.len() >= 2);

        let rehydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
                    label_id: 7,
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
                RewriteWriteEventProjection::BootstrapGraph(bootstrap.projection()),
                RewriteWriteEventProjection::EnsureCapacity(
                    RewriteEnsureCapacityProjection::from_summary(&ensure),
                ),
                RewriteWriteEventProjection::InsertEdge(RewriteInsertEdgeProjection::from_summary(
                    &insert,
                )),
            ],
        );
    }

    #[test]
    fn facade_can_collect_maintenance_candidates() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
            Some(RewriteFacadeWriteEvent::MaintenanceQueue(event)) => {
                assert_eq!(event.action, RewriteMaintenanceQueueAction::Refresh);
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
            Some(RewriteFacadeWriteEvent::MaintenanceQueue(event)) => {
                assert_eq!(event.action, RewriteMaintenanceQueueAction::Rebuild);
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
            Some(RewriteFacadeWriteEvent::MaintenanceCycle(event_summary)) => {
                let projection = RewriteMaintenanceCycleProjection::from_summary(event_summary);
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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
            .sync_base_segment_capacities_from_manager(&*facade.manager.borrow());

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
                RewriteWriteEventProjection::BootstrapGraph(RewriteBootstrapGraphProjection {
                    vertex_ordinals: vec![
                        RewriteVertexOrdinalMapping {
                            vertex_ref: src.into(),
                            forward_ordinal: 0,
                            reverse_ordinal: 0,
                        },
                        RewriteVertexOrdinalMapping {
                            vertex_ref: dst.into(),
                            forward_ordinal: 1,
                            reverse_ordinal: 1,
                        },
                    ],
                    locators: Vec::new(),
                    refreshed: RewriteRefreshedVertices::new(Vec::new(), Vec::new()),
                }),
                RewriteWriteEventProjection::MaintenanceBatch(RewriteMaintenanceBatchProjection {
                    cycles: 1,
                    queue_len_before: 0,
                    queue_len_after: 0,
                    swept_forward_segments: 1,
                    swept_reverse_segments: 1,
                    queue_storage_before: Some(RewriteMaintenanceQueueStorageProjection {
                        logical_len_bytes: 24,
                        queue_len: 0,
                        legacy_format: false,
                        format_version: Some(1),
                        stored_checksum: None,
                        computed_checksum: None,
                        checksum_valid: Some(true),
                    }),
                    queue_storage_after: Some(RewriteMaintenanceQueueStorageProjection {
                        logical_len_bytes: 24,
                        queue_len: 0,
                        legacy_format: false,
                        format_version: Some(1),
                        stored_checksum: None,
                        computed_checksum: None,
                        checksum_valid: Some(true),
                    }),
                }),
            ],
        );
    }

    #[test]
    fn facade_can_scan_property_index_directly_from_stable_memory_when_clean() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(51u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(888, "weight", &Value::Int64(5))
            .expect("set edge property");
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(&*mem_rc.borrow())
            .expect("write all");

        assert_eq!(
            facade
                .try_scan_node_ids_by_property_eq_from_stable_memory(
                    &*facade.memory.borrow(),
                    "uid",
                    &Value::Text("alice".into()),
                )
                .expect("scan node equality from stable memory"),
            vec![node_id]
        );
        assert_eq!(
            facade
                .try_scan_node_ids_by_property_from_stable_memory(&*facade.memory.borrow(), "uid",)
                .expect("scan node property from stable memory"),
            vec![node_id]
        );
        assert_eq!(
            facade
                .try_scan_edge_ids_by_property_eq_from_stable_memory(
                    &*facade.memory.borrow(),
                    "weight",
                    &Value::Int64(5),
                )
                .expect("scan edge equality from stable memory"),
            vec![888]
        );
        assert_eq!(
            facade
                .try_scan_edge_ids_by_property_from_stable_memory(
                    &*facade.memory.borrow(),
                    "weight",
                )
                .expect("scan edge property from stable memory"),
            vec![888]
        );
    }

    #[test]
    fn facade_prefers_hydrated_property_index_when_dirty() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(61u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(999, "weight", &Value::Int64(7))
            .expect("set edge property");

        assert!(facade.node_property_store_is_dirty());
        assert!(facade.edge_property_store_is_dirty());

        let m = facade.memory.borrow();
        assert_eq!(
            facade.scan_node_ids_by_property_eq_preferring_stable_memory(
                &*m,
                "uid",
                &Value::Text("alice".into()),
            ),
            vec![node_id]
        );
        assert_eq!(
            facade.scan_node_ids_by_property_preferring_stable_memory(&*m, "uid"),
            vec![node_id]
        );
        assert_eq!(
            facade.scan_edge_ids_by_property_eq_preferring_stable_memory(
                &*m,
                "weight",
                &Value::Int64(7),
            ),
            vec![999]
        );
        assert_eq!(
            facade.scan_edge_ids_by_property_preferring_stable_memory(&*m, "weight"),
            vec![999]
        );
    }

    #[test]
    fn facade_property_index_hydrate_uses_v3_equality_map_when_snapshot_empty() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let node_id = NodeId::from(41u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(777, "weight", &Value::Int64(5))
            .expect("set edge property");
        let mem_rc = Rc::clone(&facade.memory);
        facade
            .try_write_all_to_stable_memory(&*mem_rc.borrow())
            .expect("write all");

        let eq_bytes = serialize_property_equality_btree(&facade.property_equality_map);
        let equality_map =
            hydrate_property_equality_map_from_serialized_bytes(eq_bytes).expect("clone eq map");
        let image = PropertyIndexStorageImage {
            snapshot: PropertyIndexSnapshot::empty(64),
            equality_map,
        };
        write_property_index_storage_image_to_stable_memory(
            &mut *facade.manager.borrow_mut(),
            &*facade.memory.borrow(),
            &image,
        )
        .expect("overwrite property index image");

        let rehydrated = RewriteGraphPma::hydrate_from_stable_memory(
            facade.manager.borrow().clone(),
            facade.memory.borrow().clone(),
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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade
            .graph
            .insert_base_edge_pair(
                77,
                NodeId::from(1u8).into(),
                0,
                NodeId::from(2u8).into(),
                0,
                7,
            )
            .expect("seed sidecar");

        let mem_rc = Rc::clone(&facade.memory);
        let mem_guard = mem_rc.borrow();
        let mut adapter = RewriteGraphStoreAdapter::new(&mut facade, &*mem_guard);
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
                label_id: 9,
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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();
        facade
            .graph
            .insert_base_edge_pair(
                77,
                NodeId::from(1u8).into(),
                0,
                NodeId::from(2u8).into(),
                0,
                7,
            )
            .expect("seed sidecar");

        let replace_summary: RewriteGraphMutationWriteSummary<_> = facade
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
                    label_id: 9,
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
        let mut facade =
            RewriteGraphPma::hydrate_from_stable_memory(manager, memory.clone()).unwrap();

        let err = facade
            .try_rebuild_logical_locator_sidecar(&[NodeId::from(1u8).into()], &[])
            .expect_err("mismatched ids should fail");
        assert_eq!(err, RewriteGraphPmaError::InvalidLocatorInputs);
    }

    #[test]
    fn facade_can_hydrate_with_logical_locator_sidecar_in_one_step() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = RewriteGraphPma::hydrate_from_stable_memory_with_logical_locator_sidecar(
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
        let facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap empty");

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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");

        let summary: RewriteAppendVertexWriteSummary = facade
            .append_empty_vertex_pair_and_write(&memory)
            .expect("append empty vertex pair");

        assert_eq!(summary.ordinals, (0, 0));
        assert_eq!(
            summary.refreshed,
            RewriteRefreshedVertices::new(Vec::new(), Vec::new())
        );
        assert_eq!(facade.graph.forward.0.vertices.len(), 1);
        assert_eq!(facade.graph.reverse.0.vertices.len(), 1);
        assert_projected_history(
            facade.write_history(),
            vec![RewriteWriteEventProjection::BootstrapVertices(
                RewriteBootstrapVerticesProjection::from_single_summary(&summary),
            )],
        );
    }

    #[test]
    fn facade_can_append_multiple_empty_vertex_pairs_and_write() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");

        let summary: RewriteAppendVerticesWriteSummary = facade
            .append_empty_vertex_pairs_and_write(3, &memory)
            .expect("append empty vertex pairs");

        assert_eq!(summary.ordinals, vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(
            summary.refreshed,
            RewriteRefreshedVertices::new(Vec::new(), Vec::new())
        );
        assert_eq!(facade.graph.forward.0.vertices.len(), 3);
        assert_eq!(facade.graph.reverse.0.vertices.len(), 3);
    }

    #[test]
    fn facade_can_bootstrap_first_edge_between_new_vertices() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");

        let summary: RewriteBootstrapEdgeWriteSummary = facade
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
            vec![RewriteWriteEventProjection::BootstrapEdge(
                RewriteBootstrapEdgeProjection::from_facade_summary(&summary),
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");

        let summary: RewriteBootstrapGraphWriteSummary = facade
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
                RewriteVertexOrdinalMapping {
                    vertex_ref: NodeId::from(1u8).into(),
                    forward_ordinal: 0,
                    reverse_ordinal: 0,
                },
                RewriteVertexOrdinalMapping {
                    vertex_ref: NodeId::from(2u8).into(),
                    forward_ordinal: 1,
                    reverse_ordinal: 1,
                },
                RewriteVertexOrdinalMapping {
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
                RewriteEdgeLogicalLocatorMapping {
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
                RewriteEdgeLogicalLocatorMapping {
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
    fn facade_implements_rewrite_graph_store_trait() {
        fn touch_store(
            store: &mut impl RewriteGraphStore,
            memory: &impl Memory,
        ) -> RewriteGraphPmaResult<(usize, usize, usize, usize)> {
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
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let mem_guard = mem_rc.borrow();
        let counts = touch_store(&mut facade, &*mem_guard).expect("touch via trait");
        assert_eq!(counts, (4, 4, 1, 1));
        assert_eq!(
            RewriteGraphStore::formatted_last_write_event(&facade),
            Some("bootstrap-graph vertices=2 edges=1 refreshed=(1,1) fwd=[2] rev=[3]".to_owned())
        );
    }

    #[test]
    fn rewrite_graph_store_adapter_can_bootstrap_via_trait_boundary() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let mem_guard = mem_rc.borrow();
        let mut adapter = facade.bind(&*mem_guard);

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
            Some(RewriteFacadeWriteEvent::BootstrapGraph(_))
        ));
        let event_projection = match adapter.last_write_event() {
            Some(RewriteFacadeWriteEvent::BootstrapGraph(event_summary)) => {
                event_summary.projection()
            }
            other => panic!("expected bootstrap graph event, got {other:?}"),
        };
        assert_eq!(summary.projection(), event_projection);
        assert!(matches!(
            adapter.write_history(),
            [RewriteFacadeWriteEvent::BootstrapGraph(_)]
        ));
        assert_eq!(
            RewriteGraphStore::formatted_last_write_event(adapter.store),
            Some("bootstrap-graph vertices=2 edges=1 refreshed=(1,1) fwd=[0] rev=[1]".to_owned())
        );
    }

    #[test]
    fn rewrite_graph_store_adapter_can_replace_and_tombstone_edges() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let mem_guard = mem_rc.borrow();
        let mut adapter = facade.bind(&*mem_guard);

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
                label_id: 7,
            })
            .expect("replace through adapter");
        assert_eq!(replaced.mutation.0, GraphMutationPath::Base);
        let replace_projection = adapter
            .last_write_event()
            .and_then(RewriteFacadeWriteEvent::edge_projection)
            .expect("replace edge projection");
        assert_eq!(
            replace_projection.operation,
            RewriteEdgeWriteOperation::ReplaceLabel
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
            .and_then(RewriteFacadeWriteEvent::edge_projection)
            .expect("delete edge projection");
        assert_eq!(
            delete_projection.operation,
            RewriteEdgeWriteOperation::Delete
        );
        assert_eq!(delete_projection.path, GraphMutationPath::Base);
        assert_eq!(
            adapter.shared_write_history(),
            vec![
                RewriteWriteEventProjection::BootstrapGraph(bootstrap.projection()),
                RewriteWriteEventProjection::Edge(replace_projection),
                RewriteWriteEventProjection::Edge(delete_projection),
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
    fn rewrite_graph_store_adapter_can_start_batch_session() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let mem_guard = mem_rc.borrow();
        let mut adapter = facade.bind(&*mem_guard);

        let mut batch = adapter.begin_batch_mutation();
        let refreshed = batch.flush().expect("flush empty batch");
        assert_eq!(refreshed, (Vec::new(), Vec::new()));
    }

    #[test]
    fn rewrite_graph_service_trait_can_drive_bootstrap_and_flush() {
        fn use_service(
            service: &mut impl RewriteGraphService,
        ) -> RewriteGraphPmaResult<(usize, usize, bool, RewriteBootstrapGraphProjection)> {
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
                    Some(RewriteFacadeWriteEvent::BootstrapGraph(_))
                ),
                projection,
            ))
        }

        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
        let mem_rc = Rc::clone(&facade.memory);
        let mem_guard = mem_rc.borrow();
        let mut adapter = facade.bind(&*mem_guard);
        let (insert_count, history_len, has_insert_event, summary_projection) =
            use_service(&mut adapter).expect("drive via service trait");
        assert_eq!(insert_count, 1);
        assert_eq!(history_len, 1);
        assert!(has_insert_event);
        let event_projection = match adapter.last_write_event() {
            Some(RewriteFacadeWriteEvent::BootstrapGraph(event_summary)) => {
                event_summary.projection()
            }
            other => panic!("expected bootstrap graph event, got {other:?}"),
        };
        assert_eq!(summary_projection, event_projection);
        assert_eq!(
            RewriteGraphService::formatted_last_write_event(&adapter),
            Some("bootstrap-graph vertices=2 edges=1 refreshed=(1,1) fwd=[0] rev=[1]".to_owned())
        );
    }
}
