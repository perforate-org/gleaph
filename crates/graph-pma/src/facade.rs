//! Thin rewrite-facing facade over the new low-level `graph-pma` runtime.
//!
//! This module deliberately stays small. It does not hide the low-level model;
//! it only bundles the pieces that most callers would otherwise wire together
//! by hand:
//!
//! - region-manager metadata
//! - hydrated forward/reverse graph runtime state
//! - stable-memory hydration and writeback entrypoints

use std::error::Error;
use std::fmt;

use crate::stable::Memory;
use gleaph_gql::Value;
use gleaph_graph_kernel::{EdgeId, LabelId, NodeId, PropertyMap};

use crate::integration::RewriteGraphPmaKernelOverlay;
use crate::low_level::{
    forward_surface_from_layout, hydrate_surface_runtimes_from_stable_memory,
    reverse_surface_from_layout, write_surface_runtimes_to_stable_memory, BucketSizeInPages,
    EdgeEntry, EdgeInsertPath, EdgeLocator, EdgeLocatorSidecar, ExtentChain, ExtentId,
    ForwardSurfaceRuntime, GraphBatchMutationSession, GraphEnsureCapacityWriteSummary,
    GraphInsertPolicy, GraphInsertResult, GraphInsertWriteSummary, GraphMutationPath, GraphRuntime,
    HydratedSurfaceRuntimes, HydrationError, RegionKind, RegionManager, ResolvedEdgeSlot,
    ReverseSurfaceRuntime, WasmPages, WritebackError,
};
use crate::observability::{format_last_write_event, format_write_event_history};
use crate::property_index::{
    read_edge_property_index_paged_area_from_stable_memory,
    read_node_property_index_paged_area_from_stable_memory,
    read_property_index_region_header_from_stable_memory,
    read_property_index_snapshot_section_from_stable_memory,
    scan_edge_property_index_property_prefix_from_stable_memory,
    scan_edge_property_index_value_prefix_from_stable_memory,
    scan_node_property_index_property_prefix_from_stable_memory,
    scan_node_property_index_value_prefix_from_stable_memory,
    write_property_index_storage_image_to_stable_memory, PropertyIndex, PropertyIndexEntityKind,
    PropertyIndexEntry, PropertyIndexError, PropertyIndexKey, PropertyIndexNodeId,
    PropertyIndexNodeStore, PropertyIndexNodeStoreDelta, PropertyIndexNodeStoreMutationKind,
    PropertyIndexSnapshot, PropertyIndexStorageImage,
};
use crate::property_store::{
    default_property_region_chain, read_graph_property_store_from_stable_memory,
    write_graph_property_store_to_stable_memory, GraphPropertyAppendLog, PropertyKey,
    PropertyStoreError,
};

/// Thin entrypoint for the rewrite implementation of `graph-pma`.
///
/// This facade owns the region-manager metadata together with the hydrated
/// graph runtime, while keeping stable-memory access explicit at method call
/// sites. The goal is to make the rewrite usable without hiding the
/// low-level-first model we are still iterating on.
#[derive(Clone, Debug, PartialEq)]
pub struct RewriteGraphPma {
    /// Region metadata and allocator-side state for the rewrite.
    pub manager: RegionManager,
    /// In-memory forward/reverse adjacency runtime plus locator sidecar.
    pub graph: GraphRuntime,
    /// Stable-memory-backed node property store.
    pub node_property_store: GraphPropertyAppendLog,
    /// Stable-memory-backed edge property store.
    pub edge_property_store: GraphPropertyAppendLog,
    /// Derived equality index for node properties.
    pub node_property_index: PropertyIndex,
    /// Derived equality index for edge properties.
    pub edge_property_index: PropertyIndex,
    /// Persisted node-store image for the node-property equality index.
    pub node_property_index_nodes: PropertyIndexNodeStore,
    /// Persisted node-store image for the edge-property equality index.
    pub edge_property_index_nodes: PropertyIndexNodeStore,
    /// Whether the node property store has unflushed changes.
    pub node_property_store_dirty: bool,
    /// Whether the edge property store has unflushed changes.
    pub edge_property_store_dirty: bool,
    /// Most recent facade-level write event.
    pub last_write_event: Option<RewriteFacadeWriteEvent>,
    /// Recent facade-level write events in observation order.
    pub write_history: Vec<RewriteFacadeWriteEvent>,
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
pub struct RewriteGraphStoreAdapter<'a, S: RewriteGraphStore, M: Memory> {
    store: &'a mut S,
    memory: &'a M,
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

    /// Bootstraps multiple vertices and initial edges.
    fn bootstrap_vertices_and_edges(
        &mut self,
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary>;

    /// Inserts one logical edge.
    fn insert_edge_pair_with_local_rebalance(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Result<GraphInsertWriteSummary, WritebackError>;

    /// Replaces one logical edge.
    fn replace_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
    ) -> Result<
        RewriteGraphMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>,
        WritebackError,
    >;

    /// Tombstones one logical edge.
    fn tombstone_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
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
    fn manager(&self) -> &RegionManager;

    /// Returns mutable region-manager metadata.
    fn manager_mut(&mut self) -> &mut RegionManager;

    /// Returns the underlying graph runtime.
    fn graph(&self) -> &GraphRuntime;

    /// Returns mutable access to the underlying graph runtime.
    fn graph_mut(&mut self) -> &mut GraphRuntime;

    /// Returns immutable access to the stable node property store.
    fn node_property_store(&self) -> &GraphPropertyAppendLog;

    /// Returns mutable access to the stable node property store.
    fn node_property_store_mut(&mut self) -> &mut GraphPropertyAppendLog;

    /// Returns immutable access to the stable edge property store.
    fn edge_property_store(&self) -> &GraphPropertyAppendLog;

    /// Returns mutable access to the stable edge property store.
    fn edge_property_store_mut(&mut self) -> &mut GraphPropertyAppendLog;

    /// Returns the latest node properties for one semantic node id.
    fn scan_node_properties(&self, node_id: NodeId) -> PropertyMap;

    /// Returns the latest edge properties for one semantic edge id.
    fn scan_edge_properties(&self, edge_id: EdgeId) -> PropertyMap;

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

    /// Rebuilds the canonical locator sidecar from externally supplied forward-side ids.
    fn try_rebuild_locator_sidecar(
        &mut self,
        forward_vertex_ids: &[NodeId],
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
    fn bootstrap_vertices_and_edges_and_write(
        &mut self,
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary>;

    /// Inserts one logical edge and performs one local rebalance cycle first if needed.
    fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError>;

    /// Replaces one logical edge and writes back dirty state.
    fn replace_edge_pair_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
        memory: &impl Memory,
    ) -> Result<
        RewriteGraphMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>,
        WritebackError,
    >;

    /// Tombstones one logical edge and writes back dirty state.
    fn tombstone_edge_pair_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        memory: &impl Memory,
    ) -> Result<RewriteGraphMutationWriteSummary<GraphMutationPath>, WritebackError>;
}

impl<T> RewriteGraphStore for &mut T
where
    T: RewriteGraphStore + ?Sized,
{
    fn last_write_event(&self) -> Option<&RewriteFacadeWriteEvent> {
        (**self).last_write_event()
    }

    fn write_history(&self) -> &[RewriteFacadeWriteEvent] {
        (**self).write_history()
    }

    fn manager(&self) -> &RegionManager {
        (**self).manager()
    }

    fn manager_mut(&mut self) -> &mut RegionManager {
        (**self).manager_mut()
    }

    fn graph(&self) -> &GraphRuntime {
        (**self).graph()
    }

    fn graph_mut(&mut self) -> &mut GraphRuntime {
        (**self).graph_mut()
    }

    fn node_property_store(&self) -> &GraphPropertyAppendLog {
        (**self).node_property_store()
    }

    fn node_property_store_mut(&mut self) -> &mut GraphPropertyAppendLog {
        (**self).node_property_store_mut()
    }

    fn edge_property_store(&self) -> &GraphPropertyAppendLog {
        (**self).edge_property_store()
    }

    fn edge_property_store_mut(&mut self) -> &mut GraphPropertyAppendLog {
        (**self).edge_property_store_mut()
    }

    fn scan_node_properties(&self, node_id: NodeId) -> PropertyMap {
        (**self).scan_node_properties(node_id)
    }

    fn scan_edge_properties(&self, edge_id: EdgeId) -> PropertyMap {
        (**self).scan_edge_properties(edge_id)
    }

    fn scan_node_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<NodeId> {
        (**self).scan_node_ids_by_property_eq(property, value)
    }

    fn scan_node_ids_by_property(&self, property: &str) -> Vec<NodeId> {
        (**self).scan_node_ids_by_property(property)
    }

    fn scan_edge_ids_by_property(&self, property: &str) -> Vec<EdgeId> {
        (**self).scan_edge_ids_by_property(property)
    }

    fn scan_edge_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<EdgeId> {
        (**self).scan_edge_ids_by_property_eq(property, value)
    }

    fn node_property_store_is_dirty(&self) -> bool {
        (**self).node_property_store_is_dirty()
    }

    fn edge_property_store_is_dirty(&self) -> bool {
        (**self).edge_property_store_is_dirty()
    }

    fn set_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        (**self).set_node_property_value(node_id, property, value)
    }

    fn remove_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        (**self).remove_node_property_value(node_id, property)
    }

    fn set_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        (**self).set_edge_property_value(edge_id, property, value)
    }

    fn remove_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        (**self).remove_edge_property_value(edge_id, property)
    }

    fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        (**self).set_node_property_value_and_write(node_id, property, value, memory)
    }

    fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        (**self).remove_node_property_value_and_write(node_id, property, memory)
    }

    fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        (**self).set_edge_property_value_and_write(edge_id, property, value, memory)
    }

    fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        (**self).remove_edge_property_value_and_write(edge_id, property, memory)
    }

    fn try_rebuild_locator_sidecar(
        &mut self,
        forward_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> RewriteGraphPmaResult<()> {
        (**self).try_rebuild_locator_sidecar(forward_vertex_ids, forward_base_edge_ids_by_ordinal)
    }

    fn try_write_all_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<()> {
        (**self).try_write_all_to_stable_memory(memory)
    }

    fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<(Vec<usize>, Vec<usize>)> {
        (**self).try_refresh_and_write_dirty_to_stable_memory(memory)
    }

    fn append_empty_vertex_pair(&mut self) -> RewriteGraphPmaResult<(usize, usize)> {
        (**self).append_empty_vertex_pair()
    }

    fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> RewriteGraphPmaResult<Vec<(usize, usize)>> {
        (**self).append_empty_vertex_pairs(count)
    }

    fn bootstrap_vertices_and_edges_and_write(
        &mut self,
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        (**self).bootstrap_vertices_and_edges_and_write(vertex_ids, initial_edges, memory)
    }

    fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        (**self).insert_edge_pair_with_local_rebalance_and_write(
            edge_id,
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            label_id,
            forward_rebalance_vertex_ids,
            forward_base_edge_ids_by_ordinal,
            memory,
        )
    }

    fn replace_edge_pair_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
        memory: &impl Memory,
    ) -> Result<
        RewriteGraphMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>,
        WritebackError,
    > {
        (**self).replace_edge_pair_and_write(
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
            label_id,
            memory,
        )
    }

    fn tombstone_edge_pair_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        memory: &impl Memory,
    ) -> Result<RewriteGraphMutationWriteSummary<GraphMutationPath>, WritebackError> {
        (**self).tombstone_edge_pair_and_write(
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
            memory,
        )
    }
}

/// Result of one convenience mutation that also flushed dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteGraphMutationWriteSummary<T> {
    /// Mutation result returned by the underlying graph runtime.
    pub mutation: T,
    /// Vertices whose label sidecars were refreshed during writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Vertices whose label sidecars were refreshed during one facade-level writeback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteRefreshedVertices {
    /// Forward vertices whose label sidecars were refreshed during writeback.
    pub forward: Vec<usize>,
    /// Reverse vertices whose label sidecars were refreshed during writeback.
    pub reverse: Vec<usize>,
}

/// Result of appending one empty vertex slot pair and flushing the resulting dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteAppendVertexWriteSummary {
    /// Newly appended forward and reverse ordinals.
    pub ordinals: (usize, usize),
    /// Vertices whose label sidecars were refreshed during writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Result of appending multiple empty vertex slot pairs and flushing dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteAppendVerticesWriteSummary {
    /// Newly appended forward and reverse ordinals, in append order.
    pub ordinals: Vec<(usize, usize)>,
    /// Vertices whose label sidecars were refreshed during writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Surface-local ordinals assigned to one bootstrapped logical vertex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewriteVertexOrdinalMapping {
    /// Logical vertex id supplied by the caller.
    pub vertex_id: NodeId,
    /// Ordinal of the corresponding forward-surface vertex entry.
    pub forward_ordinal: usize,
    /// Ordinal of the corresponding reverse-surface vertex entry.
    pub reverse_ordinal: usize,
}

/// Result of creating two new empty vertex slots and inserting one logical edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteBootstrapEdgeWriteSummary {
    /// Ordinals assigned to the newly appended source and destination vertices.
    pub ordinals: (usize, usize),
    /// Insert result for the first edge between those vertices.
    pub insert: GraphInsertResult,
    /// Vertices whose label sidecars were refreshed during writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Locator mapping assigned to one bootstrapped logical edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewriteEdgeLocatorMapping {
    /// Semantic edge id supplied by the caller.
    pub edge_id: EdgeId,
    /// Canonical forward-side locator.
    pub canonical: EdgeLocator,
    /// Forward-surface physical locator.
    pub forward: EdgeLocator,
    /// Reverse-surface physical locator.
    pub reverse: EdgeLocator,
}

/// Result of bootstrapping multiple new vertex slots and initial edges in one write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteBootstrapGraphWriteSummary {
    /// Mapping from supplied vertex ids to newly appended surface-local ordinals.
    pub vertex_ordinals: Vec<RewriteVertexOrdinalMapping>,
    /// Insert results for the supplied initial edges, in input order.
    pub inserts: Vec<GraphInsertResult>,
    /// Locator mappings for the inserted edges, in input order.
    pub locators: Vec<RewriteEdgeLocatorMapping>,
    /// Vertices whose label sidecars were refreshed during writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Shared bootstrap observation fields used across facade and overlay summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteBootstrapGraphProjection {
    /// Mapping from logical vertex ids to rewrite ordinals.
    pub vertex_ordinals: Vec<RewriteVertexOrdinalMapping>,
    /// Locator mappings for bootstrapped edges.
    pub locators: Vec<RewriteEdgeLocatorMapping>,
    /// Vertices refreshed during the same bootstrap flow.
    pub refreshed: RewriteRefreshedVertices,
}

/// Shared per-vertex bootstrap observation fields used across facade and overlay summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteBootstrapVerticesProjection {
    /// Forward/reverse ordinals assigned during the bootstrap step.
    pub ordinals: Vec<(usize, usize)>,
    /// Vertices refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Shared per-edge bootstrap observation fields used across facade and overlay summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteBootstrapEdgeProjection {
    /// Chosen insert path for the edge pair, when insertion happened.
    pub path: Option<EdgeInsertPath>,
    /// Vertices refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteBootstrapGraphWriteSummary {
    /// Projects the facade bootstrap summary onto the fields shared with overlay bootstrap summaries.
    pub fn projection(&self) -> RewriteBootstrapGraphProjection {
        RewriteBootstrapGraphProjection {
            vertex_ordinals: self.vertex_ordinals.clone(),
            locators: self.locators.clone(),
            refreshed: self.refreshed.clone(),
        }
    }
}

impl RewriteBootstrapVerticesProjection {
    fn from_single_summary(summary: &RewriteAppendVertexWriteSummary) -> Self {
        Self {
            ordinals: vec![summary.ordinals],
            refreshed: summary.refreshed.clone(),
        }
    }

    fn from_many_summary(summary: &RewriteAppendVerticesWriteSummary) -> Self {
        Self {
            ordinals: summary.ordinals.clone(),
            refreshed: summary.refreshed.clone(),
        }
    }
}

impl RewriteBootstrapEdgeProjection {
    fn from_facade_summary(summary: &RewriteBootstrapEdgeWriteSummary) -> Self {
        let path = match summary.insert {
            GraphInsertResult::Inserted { path, .. } => Some(path),
            GraphInsertResult::RebalanceRequired(_) => None,
        };
        Self {
            path,
            refreshed: summary.refreshed.clone(),
        }
    }
}

/// Shared edge-mutation kind used across facade and overlay observability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewriteEdgeWriteOperation {
    /// Edge label replacement.
    ReplaceLabel,
    /// Edge tombstone/delete.
    Delete,
}

/// Shared edge-write observation fields used across facade and overlay summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteEdgeWriteProjection {
    /// Observed logical edge-mutation kind.
    pub operation: RewriteEdgeWriteOperation,
    /// Chosen graph-level physical path.
    pub path: GraphMutationPath,
    /// Vertices refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Shared node-delete observation fields used across facade and overlay summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteNodeDeleteProjection {
    /// Whether the node delete used detach semantics.
    pub detached: bool,
    /// Incident edge ids deleted as part of the node delete.
    pub deleted_edge_ids: Vec<EdgeId>,
    /// Edge writes observed while deleting incident edges.
    pub edge_writes: Vec<RewriteEdgeWriteProjection>,
}

/// Shared ensure-capacity observation fields used across façade histories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteEnsureCapacityProjection {
    /// Whether a local rebalance happened before the writeback.
    pub rebalanced: bool,
    /// Total displacement applied by the rebalance, if any.
    pub total_displacement: i64,
    /// Maximum side displacement applied by the rebalance, if any.
    pub max_displacement: i64,
    /// Vertices refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Shared insert-edge observation fields used across façade histories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteInsertEdgeProjection {
    /// Whether the insert actually happened.
    pub inserted: bool,
    /// Chosen insert path when the edge was inserted.
    pub path: Option<EdgeInsertPath>,
    /// Whether a local rebalance happened before the insert.
    pub rebalanced: bool,
    /// Total displacement applied by the rebalance, if any.
    pub total_displacement: i64,
    /// Maximum side displacement applied by the rebalance, if any.
    pub max_displacement: i64,
    /// Vertices refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Shared property-write observation fields used across facade and overlay summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewritePropertyWriteProjection {
    /// Property/index sections touched by the mutation itself.
    pub sections: RewritePropertyIndexTouchedSections,
    /// Incremental node-store mutation paths observed during the property update.
    pub node_store_operations: Vec<PropertyIndexNodeStoreMutationKind>,
    /// Persisted property-index node ids whose record changed.
    pub touched_node_ids: Vec<PropertyIndexNodeId>,
    /// Newly allocated property-index node ids.
    pub allocated_node_ids: Vec<PropertyIndexNodeId>,
    /// Freed property-index node ids.
    pub freed_node_ids: Vec<PropertyIndexNodeId>,
    /// Property/index sections flushed during writeback.
    pub flushed_sections: RewritePropertyIndexTouchedSections,
    /// Vertices refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// Shared projection for write events that both façade and overlay can expose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewriteWriteEventProjection {
    /// Individual vertex bootstrap event.
    BootstrapVertices(RewriteBootstrapVerticesProjection),
    /// Individual edge bootstrap event.
    BootstrapEdge(RewriteBootstrapEdgeProjection),
    /// Aggregate graph bootstrap event.
    BootstrapGraph(RewriteBootstrapGraphProjection),
    /// Ensure-capacity event.
    EnsureCapacity(RewriteEnsureCapacityProjection),
    /// Insert-edge event.
    InsertEdge(RewriteInsertEdgeProjection),
    /// Property mutation event.
    Property(RewritePropertyWriteProjection),
    /// Edge mutation event.
    Edge(RewriteEdgeWriteProjection),
    /// Node delete event.
    NodeDelete(RewriteNodeDeleteProjection),
}

/// Property-index sections touched by one property mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewritePropertyIndexTouchedSections {
    /// Stable property-store payload was updated.
    pub property_store: bool,
    /// Logical equality index changed.
    pub logical_index: bool,
    /// Persisted property-index node store changed.
    pub node_store: bool,
}

/// Observability summary for one property mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewritePropertyIndexMutationSummary {
    /// Sections touched by the mutation.
    pub sections: RewritePropertyIndexTouchedSections,
    /// Incremental node-store mutation paths observed during the property update.
    pub node_store_operations: Vec<PropertyIndexNodeStoreMutationKind>,
    /// Persisted property-index node ids whose record changed.
    pub touched_node_ids: Vec<PropertyIndexNodeId>,
    /// Newly allocated property-index node ids.
    pub allocated_node_ids: Vec<PropertyIndexNodeId>,
    /// Freed property-index node ids.
    pub freed_node_ids: Vec<PropertyIndexNodeId>,
}

/// Result of one property mutation that also flushed dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewritePropertyMutationWriteSummary {
    /// Property-index mutation summary captured before writeback.
    pub mutation: RewritePropertyIndexMutationSummary,
    /// Property/index sections flushed during writeback.
    pub flushed_sections: RewritePropertyIndexTouchedSections,
    /// Vertices whose label sidecars were refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

/// One facade-level write event recorded in observation order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewriteFacadeWriteEvent {
    /// Appending one empty vertex pair and flushing writeback.
    AppendVertex(RewriteAppendVertexWriteSummary),
    /// Appending multiple empty vertex pairs and flushing writeback.
    AppendVertices(RewriteAppendVerticesWriteSummary),
    /// Bootstrapping the first edge between new vertices and flushing writeback.
    BootstrapEdge(RewriteBootstrapEdgeWriteSummary),
    /// Bootstrapping vertices plus initial edges and flushing writeback.
    BootstrapGraph(RewriteBootstrapGraphWriteSummary),
    /// Property mutation write observed through the facade.
    Property(RewritePropertyMutationWriteSummary),
    /// Local-capacity prepare/write observed through the facade.
    EnsureCapacity(GraphEnsureCapacityWriteSummary),
    /// Edge insert observed through the facade.
    InsertEdge(GraphInsertWriteSummary),
    /// Edge replace observed through the facade.
    ReplaceEdge(RewriteGraphMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>),
    /// Edge tombstone/delete observed through the facade.
    DeleteEdge(RewriteGraphMutationWriteSummary<GraphMutationPath>),
}

impl RewriteFacadeWriteEvent {
    /// Projects one façade write event onto zero or more shared event projections.
    pub fn shared_projections(&self) -> Vec<RewriteWriteEventProjection> {
        self.shared_projection().into_iter().collect()
    }

    /// Projects façade write events onto the shared cross-surface event vocabulary.
    pub fn shared_projection(&self) -> Option<RewriteWriteEventProjection> {
        match self {
            Self::AppendVertex(summary) => Some(RewriteWriteEventProjection::BootstrapVertices(
                RewriteBootstrapVerticesProjection::from_single_summary(summary),
            )),
            Self::AppendVertices(summary) => Some(RewriteWriteEventProjection::BootstrapVertices(
                RewriteBootstrapVerticesProjection::from_many_summary(summary),
            )),
            Self::BootstrapEdge(summary) => Some(RewriteWriteEventProjection::BootstrapEdge(
                RewriteBootstrapEdgeProjection::from_facade_summary(summary),
            )),
            Self::BootstrapGraph(summary) => Some(RewriteWriteEventProjection::BootstrapGraph(
                summary.projection(),
            )),
            Self::EnsureCapacity(summary) => Some(RewriteWriteEventProjection::EnsureCapacity(
                RewriteEnsureCapacityProjection::from_summary(summary),
            )),
            Self::InsertEdge(summary) => Some(RewriteWriteEventProjection::InsertEdge(
                RewriteInsertEdgeProjection::from_summary(summary),
            )),
            Self::Property(summary) => {
                Some(RewriteWriteEventProjection::Property(summary.projection()))
            }
            Self::ReplaceEdge(_) | Self::DeleteEdge(_) => self
                .edge_projection()
                .map(RewriteWriteEventProjection::Edge),
        }
    }

    /// Projects facade edge write events onto the fields shared with overlay edge summaries.
    pub fn edge_projection(&self) -> Option<RewriteEdgeWriteProjection> {
        match self {
            Self::ReplaceEdge(summary) => Some(RewriteEdgeWriteProjection {
                operation: RewriteEdgeWriteOperation::ReplaceLabel,
                path: summary.mutation.0,
                refreshed: summary.refreshed.clone(),
            }),
            Self::DeleteEdge(summary) => Some(RewriteEdgeWriteProjection {
                operation: RewriteEdgeWriteOperation::Delete,
                path: summary.mutation,
                refreshed: summary.refreshed.clone(),
            }),
            _ => None,
        }
    }

    /// Projects facade property write events onto the fields shared with overlay property summaries.
    pub fn property_projection(&self) -> Option<RewritePropertyWriteProjection> {
        match self {
            Self::Property(summary) => Some(summary.projection()),
            _ => None,
        }
    }
}

const FACADE_WRITE_HISTORY_LIMIT: usize = 16;

/// Facade-level error type for the rewrite entrypoint.
///
/// This keeps the higher-level facade ergonomic without erasing the low-level
/// failure modes that still matter during the rewrite phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewriteGraphPmaError {
    /// Stable-memory hydration failed.
    Hydration(HydrationError),
    /// Stable-memory writeback failed.
    Writeback(WritebackError),
    /// Property-store hydration or writeback failed.
    PropertyStore(PropertyStoreError),
    /// Property-index hydration or writeback failed.
    PropertyIndex(PropertyIndexError),
    /// Caller-supplied semantic edge ids did not match the current forward-side layout.
    InvalidLocatorInputs,
}

/// Facade-level result alias for the rewrite entrypoint.
pub type RewriteGraphPmaResult<T> = Result<T, RewriteGraphPmaError>;

impl fmt::Display for RewriteGraphPmaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hydration(err) => write!(f, "rewrite graph-pma hydration failed: {err}"),
            Self::Writeback(err) => write!(f, "rewrite graph-pma writeback failed: {err}"),
            Self::PropertyStore(err) => write!(f, "rewrite property-store operation failed: {err}"),
            Self::PropertyIndex(err) => write!(f, "rewrite property-index operation failed: {err}"),
            Self::InvalidLocatorInputs => {
                write!(f, "invalid locator rebuild inputs for forward surface")
            }
        }
    }
}

impl Error for RewriteGraphPmaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Hydration(err) => Some(err),
            Self::Writeback(err) => Some(err),
            Self::PropertyStore(err) => Some(err),
            Self::PropertyIndex(err) => Some(err),
            Self::InvalidLocatorInputs => None,
        }
    }
}

impl From<HydrationError> for RewriteGraphPmaError {
    fn from(value: HydrationError) -> Self {
        Self::Hydration(value)
    }
}

impl From<WritebackError> for RewriteGraphPmaError {
    fn from(value: WritebackError) -> Self {
        Self::Writeback(value)
    }
}

impl From<PropertyStoreError> for RewriteGraphPmaError {
    fn from(value: PropertyStoreError) -> Self {
        Self::PropertyStore(value)
    }
}

impl From<PropertyIndexError> for RewriteGraphPmaError {
    fn from(value: PropertyIndexError) -> Self {
        Self::PropertyIndex(value)
    }
}

impl RewriteRefreshedVertices {
    /// Builds one refreshed-vertex summary from forward/reverse lists.
    pub fn new(forward: Vec<usize>, reverse: Vec<usize>) -> Self {
        Self { forward, reverse }
    }

    /// Builds one refreshed-vertex summary by cloning borrowed forward/reverse lists.
    pub fn from_slices(forward: &[usize], reverse: &[usize]) -> Self {
        Self {
            forward: forward.to_vec(),
            reverse: reverse.to_vec(),
        }
    }
}

impl RewritePropertyIndexMutationSummary {
    fn from_delta(
        delta: PropertyIndexNodeStoreDelta,
        node_store_operations: Vec<PropertyIndexNodeStoreMutationKind>,
    ) -> Self {
        let node_store = !delta.touched_node_ids.is_empty();
        Self {
            sections: RewritePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store,
            },
            node_store_operations,
            touched_node_ids: delta.touched_node_ids,
            allocated_node_ids: delta.allocated_node_ids,
            freed_node_ids: delta.freed_node_ids,
        }
    }
}

impl RewritePropertyMutationWriteSummary {
    /// Projects the property write summary onto the fields shared across facade and overlay views.
    pub fn projection(&self) -> RewritePropertyWriteProjection {
        RewritePropertyWriteProjection {
            sections: self.mutation.sections,
            node_store_operations: self.mutation.node_store_operations.clone(),
            touched_node_ids: self.mutation.touched_node_ids.clone(),
            allocated_node_ids: self.mutation.allocated_node_ids.clone(),
            freed_node_ids: self.mutation.freed_node_ids.clone(),
            flushed_sections: self.flushed_sections,
            refreshed: self.refreshed.clone(),
        }
    }

    fn from_mutation_and_refresh(
        mutation: RewritePropertyIndexMutationSummary,
        refreshed_forward_vertices: Vec<usize>,
        refreshed_reverse_vertices: Vec<usize>,
    ) -> Self {
        Self {
            flushed_sections: mutation.sections,
            mutation,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        }
    }
}

impl RewriteEnsureCapacityProjection {
    fn from_summary(summary: &GraphEnsureCapacityWriteSummary) -> Self {
        let (total_displacement, max_displacement) = summary
            .rebalance
            .as_ref()
            .map(|rebalance| {
                (
                    rebalance.apply.total_displacement(),
                    rebalance.apply.max_displacement(),
                )
            })
            .unwrap_or((0, 0));
        Self {
            rebalanced: summary.rebalanced,
            total_displacement,
            max_displacement,
            refreshed: RewriteRefreshedVertices::new(
                summary.refreshed_forward_vertices.clone(),
                summary.refreshed_reverse_vertices.clone(),
            ),
        }
    }
}

impl RewriteInsertEdgeProjection {
    fn from_summary(summary: &GraphInsertWriteSummary) -> Self {
        let path = summary.insert.as_ref().and_then(|insert| match insert {
            GraphInsertResult::Inserted { path, .. } => Some(*path),
            GraphInsertResult::RebalanceRequired(_) => None,
        });
        let (total_displacement, max_displacement) = summary
            .rebalance
            .as_ref()
            .map(|rebalance| {
                (
                    rebalance.apply.total_displacement(),
                    rebalance.apply.max_displacement(),
                )
            })
            .unwrap_or((0, 0));
        Self {
            inserted: summary.insert.is_some(),
            path,
            rebalanced: summary.rebalance.is_some(),
            total_displacement,
            max_displacement,
            refreshed: RewriteRefreshedVertices::new(
                summary.refreshed_forward_vertices.clone(),
                summary.refreshed_reverse_vertices.clone(),
            ),
        }
    }
}

impl RewriteGraphPma {
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
}

impl RewriteGraphPma {
    /// Builds the bytes written to the property-index region on flush.
    ///
    /// The on-disk PIDX snapshot is intentionally **empty** while paged node-store sections carry
    /// the tree. [`PropertyIndexStorageImage::normalized`] during hydration reconstructs logical
    /// [`PropertyIndex`] values from those stores — callers must not infer “no index” from the
    /// empty snapshot alone (see [`PropertyIndexSnapshot`] / [`PropertyIndexStorageImage`] docs).
    fn compact_property_index_storage_image(&self) -> PropertyIndexStorageImage {
        const BRANCHING_FACTOR: u16 = 64;
        PropertyIndexStorageImage {
            snapshot: PropertyIndexSnapshot::empty(BRANCHING_FACTOR),
            node_store: self.node_property_index_nodes.clone(),
            edge_store: self.edge_property_index_nodes.clone(),
        }
    }

    fn load_property_index_image_from_stable_memory(
        manager: &RegionManager,
        memory: &impl Memory,
        page_size_bytes: u32,
    ) -> Option<PropertyIndexStorageImage> {
        const BRANCHING_FACTOR: u16 = 64;

        if read_property_index_region_header_from_stable_memory(manager, memory).is_ok() {
            let snapshot = read_property_index_snapshot_section_from_stable_memory(manager, memory)
                .unwrap_or_else(|_| PropertyIndexSnapshot::empty(BRANCHING_FACTOR));
            let node_store =
                read_node_property_index_paged_area_from_stable_memory(manager, memory)
                    .unwrap_or_else(|_| PropertyIndexNodeStore::new(page_size_bytes));
            let edge_store =
                read_edge_property_index_paged_area_from_stable_memory(manager, memory)
                    .unwrap_or_else(|_| PropertyIndexNodeStore::new(page_size_bytes));
            return Some(PropertyIndexStorageImage::from_sectioned_parts(
                snapshot,
                node_store,
                edge_store,
                BRANCHING_FACTOR,
                page_size_bytes,
            ));
        }

        None
    }

    /// Bundles an existing region manager and graph runtime into one facade.
    pub fn new(manager: RegionManager, graph: GraphRuntime) -> Self {
        Self {
            manager,
            graph,
            node_property_store: GraphPropertyAppendLog::default(),
            edge_property_store: GraphPropertyAppendLog::default(),
            node_property_index: PropertyIndex::new(64),
            edge_property_index: PropertyIndex::new(64),
            node_property_index_nodes: PropertyIndexNodeStore::new(4096),
            edge_property_index_nodes: PropertyIndexNodeStore::new(4096),
            node_property_store_dirty: false,
            edge_property_store_dirty: false,
            last_write_event: None,
            write_history: Vec::new(),
        }
    }

    /// Bootstraps one empty rewrite graph with the default bucket granularity.
    pub fn bootstrap_empty(memory: &impl Memory) -> RewriteGraphPmaResult<Self> {
        Self::bootstrap_empty_with_bucket_size(BucketSizeInPages::DEFAULT, memory)
    }

    /// Bootstraps one empty rewrite graph with an explicit bucket granularity.
    pub fn bootstrap_empty_with_bucket_size(
        bucket_size_in_pages: BucketSizeInPages,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<Self> {
        let mut manager = RegionManager::with_bucket_size(bucket_size_in_pages);
        Self::define_empty_surface_regions(&mut manager, crate::low_level::SurfaceKind::Forward);
        Self::define_empty_surface_regions(&mut manager, crate::low_level::SurfaceKind::Reverse);
        Self::define_empty_property_regions(&mut manager);

        let forward = ForwardSurfaceRuntime::without_overflow(
            forward_surface_from_layout(&manager.layout)?,
            Vec::new(),
        );
        let reverse = ReverseSurfaceRuntime::without_overflow(
            reverse_surface_from_layout(&manager.layout)?,
            Vec::new(),
        );
        let mut facade = Self::new(
            manager,
            GraphRuntime::new(forward, reverse, EdgeLocatorSidecar::new()),
        );
        facade.try_write_all_to_stable_memory(memory)?;
        Ok(facade)
    }

    /// Creates one facade from already-hydrated directional runtimes.
    pub fn from_hydrated_runtimes(
        manager: RegionManager,
        runtimes: HydratedSurfaceRuntimes,
    ) -> Self {
        Self::new(
            manager,
            GraphRuntime::new(
                runtimes.forward,
                runtimes.reverse,
                EdgeLocatorSidecar::new(),
            ),
        )
    }

    /// Creates one facade from hydrated runtimes and an explicit insert policy.
    pub fn from_hydrated_runtimes_with_insert_policy(
        manager: RegionManager,
        runtimes: HydratedSurfaceRuntimes,
        insert_policy: GraphInsertPolicy,
    ) -> Self {
        Self::new(
            manager,
            GraphRuntime::with_insert_policy(
                runtimes.forward,
                runtimes.reverse,
                EdgeLocatorSidecar::new(),
                insert_policy,
            ),
        )
    }

    /// Hydrates forward/reverse runtimes from stable memory and builds a facade.
    ///
    /// The locator sidecar starts empty. Callers that already know the
    /// canonical forward-side semantic edge ids can repopulate it after
    /// hydration using the lower-level sidecar helpers.
    ///
    /// Property indices are loaded through [`PropertyIndexStorageImage::from_sectioned_parts`],
    /// which normalizes an **empty on-disk logical snapshot** against non-empty node stores so
    /// in-memory `node_property_index` / `edge_property_index` match persisted pages.
    pub fn hydrate_from_stable_memory(
        manager: RegionManager,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<Self> {
        let runtimes = hydrate_surface_runtimes_from_stable_memory(&manager, memory)?;
        let node_property_store = read_graph_property_store_from_stable_memory(
            &manager,
            memory,
            RegionKind::NodePropertyStore,
        )?;
        let edge_property_store = read_graph_property_store_from_stable_memory(
            &manager,
            memory,
            RegionKind::EdgePropertyStore,
        )?;
        let mut facade = Self::from_hydrated_runtimes(manager, runtimes);
        facade.node_property_store = node_property_store;
        facade.edge_property_store = edge_property_store;
        if let Some(index_image) = Self::load_property_index_image_from_stable_memory(
            &facade.manager,
            memory,
            facade.property_index_page_size_bytes(),
        ) {
            facade.node_property_index = index_image.snapshot.node_index;
            facade.edge_property_index = index_image.snapshot.edge_index;
            facade.node_property_index_nodes = index_image.node_store;
            facade.edge_property_index_nodes = index_image.edge_store;
        }
        if facade.node_property_index.header.entry_count == 0
            && facade.edge_property_index.header.entry_count == 0
            && (!facade.node_property_store.records.is_empty()
                || !facade.edge_property_store.records.is_empty())
        {
            facade.rebuild_property_indices()?;
        }
        facade.node_property_store_dirty = false;
        facade.edge_property_store_dirty = false;
        Ok(facade)
    }

    /// Hydrates forward/reverse runtimes from stable memory with an explicit insert policy.
    pub fn hydrate_from_stable_memory_with_insert_policy(
        manager: RegionManager,
        memory: &impl Memory,
        insert_policy: GraphInsertPolicy,
    ) -> RewriteGraphPmaResult<Self> {
        let runtimes = hydrate_surface_runtimes_from_stable_memory(&manager, memory)?;
        let node_property_store = read_graph_property_store_from_stable_memory(
            &manager,
            memory,
            RegionKind::NodePropertyStore,
        )?;
        let edge_property_store = read_graph_property_store_from_stable_memory(
            &manager,
            memory,
            RegionKind::EdgePropertyStore,
        )?;
        let mut facade =
            Self::from_hydrated_runtimes_with_insert_policy(manager, runtimes, insert_policy);
        facade.node_property_store = node_property_store;
        facade.edge_property_store = edge_property_store;
        if let Some(index_image) = Self::load_property_index_image_from_stable_memory(
            &facade.manager,
            memory,
            facade.property_index_page_size_bytes(),
        ) {
            facade.node_property_index = index_image.snapshot.node_index;
            facade.edge_property_index = index_image.snapshot.edge_index;
            facade.node_property_index_nodes = index_image.node_store;
            facade.edge_property_index_nodes = index_image.edge_store;
        }
        if facade.node_property_index.header.entry_count == 0
            && facade.edge_property_index.header.entry_count == 0
            && (!facade.node_property_store.records.is_empty()
                || !facade.edge_property_store.records.is_empty())
        {
            facade.rebuild_property_indices()?;
        }
        facade.node_property_store_dirty = false;
        facade.edge_property_store_dirty = false;
        Ok(facade)
    }

    /// Hydrates one rewrite facade from stable memory using the facade-level result type.
    pub fn try_hydrate_from_stable_memory(
        manager: RegionManager,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<Self> {
        Self::hydrate_from_stable_memory(manager, memory)
    }

    /// Hydrates one rewrite facade with an explicit insert policy using the facade-level result type.
    pub fn try_hydrate_from_stable_memory_with_insert_policy(
        manager: RegionManager,
        memory: &impl Memory,
        insert_policy: GraphInsertPolicy,
    ) -> RewriteGraphPmaResult<Self> {
        Self::hydrate_from_stable_memory_with_insert_policy(manager, memory, insert_policy)
    }

    /// Hydrates one facade and immediately rebuilds the canonical locator sidecar.
    pub fn hydrate_from_stable_memory_with_locator_sidecar(
        manager: RegionManager,
        memory: &impl Memory,
        forward_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> RewriteGraphPmaResult<Self> {
        let mut facade = Self::try_hydrate_from_stable_memory(manager, memory)?;
        facade.try_rebuild_locator_sidecar(forward_vertex_ids, forward_base_edge_ids_by_ordinal)?;
        Ok(facade)
    }

    /// Hydrates one facade with an explicit insert policy and immediately rebuilds
    /// the canonical locator sidecar.
    pub fn hydrate_from_stable_memory_with_insert_policy_and_locator_sidecar(
        manager: RegionManager,
        memory: &impl Memory,
        insert_policy: GraphInsertPolicy,
        forward_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> RewriteGraphPmaResult<Self> {
        let mut facade = Self::try_hydrate_from_stable_memory_with_insert_policy(
            manager,
            memory,
            insert_policy,
        )?;
        facade.try_rebuild_locator_sidecar(forward_vertex_ids, forward_base_edge_ids_by_ordinal)?;
        Ok(facade)
    }

    /// Returns the region-manager metadata.
    pub const fn manager(&self) -> &RegionManager {
        &self.manager
    }

    /// Returns mutable access to the region-manager metadata.
    pub fn manager_mut(&mut self) -> &mut RegionManager {
        &mut self.manager
    }

    /// Returns the graph runtime.
    pub const fn graph(&self) -> &GraphRuntime {
        &self.graph
    }

    /// Returns mutable access to the graph runtime.
    pub fn graph_mut(&mut self) -> &mut GraphRuntime {
        &mut self.graph
    }

    /// Returns immutable access to the stable node property store.
    pub fn node_property_store(&self) -> &GraphPropertyAppendLog {
        &self.node_property_store
    }

    /// Returns mutable access to the stable node property store.
    pub fn node_property_store_mut(&mut self) -> &mut GraphPropertyAppendLog {
        &mut self.node_property_store
    }

    /// Returns immutable access to the stable edge property store.
    pub fn edge_property_store(&self) -> &GraphPropertyAppendLog {
        &self.edge_property_store
    }

    /// Returns mutable access to the stable edge property store.
    pub fn edge_property_store_mut(&mut self) -> &mut GraphPropertyAppendLog {
        &mut self.edge_property_store
    }

    /// Returns the latest node properties for one semantic node id.
    pub fn scan_node_properties(&self, node_id: NodeId) -> PropertyMap {
        self.node_property_store
            .scan_entity(crate::PropertyEntityKind::Node, u64::from(node_id))
    }

    /// Returns the latest edge properties for one semantic edge id.
    pub fn scan_edge_properties(&self, edge_id: EdgeId) -> PropertyMap {
        self.edge_property_store
            .scan_entity(crate::PropertyEntityKind::Edge, edge_id)
    }

    /// Returns node ids matching one exact equality property predicate.
    pub fn scan_node_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<NodeId> {
        let encoded_value = value
            .to_stable_bytes()
            .expect("Value must encode to stable bytes");
        self.node_property_index_nodes
            .scan_value_prefix_direct(
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
        self.node_property_index_nodes
            .scan_property_prefix_direct(PropertyIndexEntityKind::VertexNode, property)
            .into_iter()
            .filter_map(|(key, _)| NodeId::try_from(key.entity_id).ok())
            .collect()
    }

    /// Returns edge ids that have any binding for the given property name.
    pub fn scan_edge_ids_by_property(&self, property: &str) -> Vec<EdgeId> {
        self.edge_property_index_nodes
            .scan_property_prefix_direct(PropertyIndexEntityKind::VertexEdge, property)
            .into_iter()
            .map(|(key, _)| key.entity_id)
            .collect()
    }

    /// Returns edge ids matching one exact equality property predicate.
    pub fn scan_edge_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<EdgeId> {
        let encoded_value = value
            .to_stable_bytes()
            .expect("Value must encode to stable bytes");
        self.edge_property_index_nodes
            .scan_value_prefix_direct(
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
            .to_stable_bytes()
            .expect("Value must encode to stable bytes");
        Ok(scan_node_property_index_value_prefix_from_stable_memory(
            &self.manager,
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
            &self.manager,
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
            .to_stable_bytes()
            .expect("Value must encode to stable bytes");
        Ok(scan_edge_property_index_value_prefix_from_stable_memory(
            &self.manager,
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
            &self.manager,
            memory,
            property,
        )?
        .into_iter()
        .map(|(key, _)| key.entity_id)
        .collect())
    }

    /// Returns node ids matching one equality predicate, preferring stable-memory direct scan when clean.
    pub fn scan_node_ids_by_property_eq_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
        value: &Value,
    ) -> Vec<NodeId> {
        if !self.node_property_store_dirty {
            if let Ok(ids) =
                self.try_scan_node_ids_by_property_eq_from_stable_memory(memory, property, value)
            {
                return ids;
            }
        }
        self.scan_node_ids_by_property_eq(property, value)
    }

    /// Returns node ids that have any binding for the given property, preferring stable-memory direct scan when clean.
    pub fn scan_node_ids_by_property_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
    ) -> Vec<NodeId> {
        if !self.node_property_store_dirty {
            if let Ok(ids) = self.try_scan_node_ids_by_property_from_stable_memory(memory, property)
            {
                return ids;
            }
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
        if !self.edge_property_store_dirty {
            if let Ok(ids) =
                self.try_scan_edge_ids_by_property_eq_from_stable_memory(memory, property, value)
            {
                return ids;
            }
        }
        self.scan_edge_ids_by_property_eq(property, value)
    }

    /// Returns edge ids that have any binding for the given property, preferring stable-memory direct scan when clean.
    pub fn scan_edge_ids_by_property_preferring_stable_memory(
        &self,
        memory: &impl Memory,
        property: &str,
    ) -> Vec<EdgeId> {
        if !self.edge_property_store_dirty {
            if let Ok(ids) = self.try_scan_edge_ids_by_property_from_stable_memory(memory, property)
            {
                return ids;
            }
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

    fn property_index_page_size_bytes(&self) -> u32 {
        u32::try_from(self.manager.bucket_size_bytes()).unwrap_or(4096)
    }

    fn sync_node_property_index_node_store(&mut self) {
        let page_size_bytes = self.property_index_page_size_bytes();
        self.node_property_index_nodes =
            PropertyIndexNodeStore::from_index(&self.node_property_index, page_size_bytes);
    }

    fn sync_edge_property_index_node_store(&mut self) {
        let page_size_bytes = self.property_index_page_size_bytes();
        self.edge_property_index_nodes =
            PropertyIndexNodeStore::from_index(&self.edge_property_index, page_size_bytes);
    }

    fn sync_property_index_node_stores(&mut self) {
        self.sync_node_property_index_node_store();
        self.sync_edge_property_index_node_store();
    }

    /// Appends or overwrites one node property in the stable property store.
    pub fn set_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        let _ = self.remove_node_property_index_binding_with_kind(node_id, property);
        self.node_property_store
            .set(PropertyKey::node(node_id, property), value.clone())?;
        let _ = self.insert_node_property_index_binding_with_kind(node_id, property, value);
        self.node_property_store_dirty = true;
        Ok(())
    }

    /// Appends or overwrites one node property and reports touched index sections/node ids.
    pub fn set_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError> {
        let before = self.node_property_index_nodes.clone();
        let mut node_store_operations = Vec::new();
        if let Some(kind) = self.remove_node_property_index_binding_with_kind(node_id, property) {
            node_store_operations.push(kind);
        }
        self.node_property_store
            .set(PropertyKey::node(node_id, property), value.clone())?;
        node_store_operations.push(
            self.insert_node_property_index_binding_with_kind(node_id, property, value)
                .1,
        );
        self.node_property_store_dirty = true;
        Ok(RewritePropertyIndexMutationSummary::from_delta(
            self.node_property_index_nodes.diff_against(&before),
            node_store_operations,
        ))
    }

    /// Appends one node-property tombstone in the stable property store.
    pub fn remove_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        let _ = self.remove_node_property_index_binding_with_kind(node_id, property);
        self.node_property_store
            .remove(PropertyKey::node(node_id, property))?;
        self.node_property_store_dirty = true;
        Ok(())
    }

    /// Appends one node-property tombstone and reports touched index sections/node ids.
    pub fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError> {
        let before = self.node_property_index_nodes.clone();
        let node_store_operations = self
            .remove_node_property_index_binding_with_kind(node_id, property)
            .into_iter()
            .collect();
        self.node_property_store
            .remove(PropertyKey::node(node_id, property))?;
        self.node_property_store_dirty = true;
        Ok(RewritePropertyIndexMutationSummary::from_delta(
            self.node_property_index_nodes.diff_against(&before),
            node_store_operations,
        ))
    }

    /// Appends or overwrites one edge property in the stable property store.
    pub fn set_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        let _ = self.remove_edge_property_index_binding_with_kind(edge_id, property);
        self.edge_property_store
            .set(PropertyKey::edge(edge_id, property), value.clone())?;
        let _ = self.insert_edge_property_index_binding_with_kind(edge_id, property, value);
        self.edge_property_store_dirty = true;
        Ok(())
    }

    /// Appends or overwrites one edge property and reports touched index sections/node ids.
    pub fn set_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError> {
        let before = self.edge_property_index_nodes.clone();
        let mut node_store_operations = Vec::new();
        if let Some(kind) = self.remove_edge_property_index_binding_with_kind(edge_id, property) {
            node_store_operations.push(kind);
        }
        self.edge_property_store
            .set(PropertyKey::edge(edge_id, property), value.clone())?;
        node_store_operations.push(
            self.insert_edge_property_index_binding_with_kind(edge_id, property, value)
                .1,
        );
        self.edge_property_store_dirty = true;
        Ok(RewritePropertyIndexMutationSummary::from_delta(
            self.edge_property_index_nodes.diff_against(&before),
            node_store_operations,
        ))
    }

    /// Appends one edge-property tombstone in the stable property store.
    pub fn remove_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        let _ = self.remove_edge_property_index_binding_with_kind(edge_id, property);
        self.edge_property_store
            .remove(PropertyKey::edge(edge_id, property))?;
        self.edge_property_store_dirty = true;
        Ok(())
    }

    /// Appends one edge-property tombstone and reports touched index sections/node ids.
    pub fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError> {
        let before = self.edge_property_index_nodes.clone();
        let node_store_operations = self
            .remove_edge_property_index_binding_with_kind(edge_id, property)
            .into_iter()
            .collect();
        self.edge_property_store
            .remove(PropertyKey::edge(edge_id, property))?;
        self.edge_property_store_dirty = true;
        Ok(RewritePropertyIndexMutationSummary::from_delta(
            self.edge_property_index_nodes.diff_against(&before),
            node_store_operations,
        ))
    }

    /// Appends or overwrites one node property, then flushes dirty state.
    pub fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        let mutation = self.set_node_property_value_with_summary(node_id, property, value)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewritePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(RewriteFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    /// Appends one node-property tombstone, then flushes dirty state.
    pub fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        let mutation = self.remove_node_property_value_with_summary(node_id, property)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewritePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(RewriteFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    /// Appends or overwrites one edge property, then flushes dirty state.
    pub fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        let mutation = self.set_edge_property_value_with_summary(edge_id, property, value)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewritePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(RewriteFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    /// Appends one edge-property tombstone, then flushes dirty state.
    pub fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        let mutation = self.remove_edge_property_value_with_summary(edge_id, property)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewritePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(RewriteFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    fn rebuild_property_indices(&mut self) -> Result<(), PropertyStoreError> {
        self.node_property_index = PropertyIndex::new(64);
        self.edge_property_index = PropertyIndex::new(64);

        for (key, value) in self.node_property_store.latest_state() {
            if let Some(value) = value {
                let node_id = NodeId::try_from(key.entity_id)
                    .map_err(|_| PropertyStoreError::LengthOverflow)?;
                let _ = self.insert_node_property_index_binding_with_kind(
                    node_id,
                    &key.property_name,
                    &value,
                );
            }
        }

        for (key, value) in self.edge_property_store.latest_state() {
            if let Some(value) = value {
                let _ = self.insert_edge_property_index_binding_with_kind(
                    key.entity_id,
                    &key.property_name,
                    &value,
                );
            }
        }

        self.sync_property_index_node_stores();

        Ok(())
    }

    fn insert_node_property_index_binding_with_kind(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> (PropertyIndexKey, PropertyIndexNodeStoreMutationKind) {
        let key = PropertyIndexKey::node(
            node_id,
            property,
            value
                .to_stable_bytes()
                .expect("Value must encode to stable bytes"),
        );
        self.node_property_index
            .insert(key.clone(), PropertyIndexEntry::empty());
        let operation = self
            .node_property_index_nodes
            .upsert_leaf_chain_entry_with_kind(key.clone(), PropertyIndexEntry::empty())
            .unwrap_or_else(|| {
                self.sync_node_property_index_node_store();
                PropertyIndexNodeStoreMutationKind::Rebuild
            });
        (key, operation)
    }

    fn remove_node_property_index_binding_with_kind(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Option<PropertyIndexNodeStoreMutationKind> {
        if let Some(old_value) = self
            .node_property_store
            .get_node_property(node_id, property)
        {
            let key = PropertyIndexKey::node(
                node_id,
                property,
                old_value
                    .to_stable_bytes()
                    .expect("Value must encode to stable bytes"),
            );
            self.node_property_index.remove(&key);
            return Some(
                self.node_property_index_nodes
                    .remove_leaf_chain_entry_with_kind(&key)
                    .unwrap_or_else(|| {
                        self.sync_node_property_index_node_store();
                        PropertyIndexNodeStoreMutationKind::Rebuild
                    }),
            );
        }
        None
    }

    fn insert_edge_property_index_binding_with_kind(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> (PropertyIndexKey, PropertyIndexNodeStoreMutationKind) {
        let key = PropertyIndexKey::edge(
            edge_id,
            property,
            value
                .to_stable_bytes()
                .expect("Value must encode to stable bytes"),
        );
        self.edge_property_index
            .insert(key.clone(), PropertyIndexEntry::empty());
        let operation = self
            .edge_property_index_nodes
            .upsert_leaf_chain_entry_with_kind(key.clone(), PropertyIndexEntry::empty())
            .unwrap_or_else(|| {
                self.sync_edge_property_index_node_store();
                PropertyIndexNodeStoreMutationKind::Rebuild
            });
        (key, operation)
    }

    fn remove_edge_property_index_binding_with_kind(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Option<PropertyIndexNodeStoreMutationKind> {
        if let Some(old_value) = self
            .edge_property_store
            .get_edge_property(edge_id, property)
        {
            let key = PropertyIndexKey::edge(
                edge_id,
                property,
                old_value
                    .to_stable_bytes()
                    .expect("Value must encode to stable bytes"),
            );
            self.edge_property_index.remove(&key);
            return Some(
                self.edge_property_index_nodes
                    .remove_leaf_chain_entry_with_kind(&key)
                    .unwrap_or_else(|| {
                        self.sync_edge_property_index_node_store();
                        PropertyIndexNodeStoreMutationKind::Rebuild
                    }),
            );
        }
        None
    }

    /// Replaces the canonical locator sidecar by rebuilding it from externally
    /// supplied forward-surface semantic edge ids.
    pub fn rebuild_locator_sidecar(
        &mut self,
        forward_vertex_ids: &[gleaph_graph_kernel::NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<()> {
        self.graph.locator_sidecar = self
            .graph
            .forward
            .0
            .build_locator_sidecar_from_vertex_base_ids(
                forward_vertex_ids,
                forward_base_edge_ids_by_ordinal,
            )?;
        Some(())
    }

    /// Rebuilds the canonical locator sidecar using the facade-level result type.
    pub fn try_rebuild_locator_sidecar(
        &mut self,
        forward_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> RewriteGraphPmaResult<()> {
        self.rebuild_locator_sidecar(forward_vertex_ids, forward_base_edge_ids_by_ordinal)
            .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)
    }

    /// Writes the full forward/reverse runtime state back to stable memory.
    ///
    /// The property-index region is written in compact form: logical PIDX bytes are omitted (empty
    /// [`PropertyIndexSnapshot`]) in favour of paged node stores (see [`PropertyIndexStorageImage`]).
    pub fn write_all_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<()> {
        let runtimes =
            HydratedSurfaceRuntimes::new(self.graph.forward.clone(), self.graph.reverse.clone());
        write_surface_runtimes_to_stable_memory(&self.manager, memory, &runtimes)?;
        write_graph_property_store_to_stable_memory(
            &mut self.manager,
            memory,
            RegionKind::NodePropertyStore,
            &self.node_property_store,
        )?;
        write_graph_property_store_to_stable_memory(
            &mut self.manager,
            memory,
            RegionKind::EdgePropertyStore,
            &self.edge_property_store,
        )?;
        self.sync_property_index_node_stores();
        let image = self.compact_property_index_storage_image();
        write_property_index_storage_image_to_stable_memory(&mut self.manager, memory, &image)?;
        self.node_property_store_dirty = false;
        self.edge_property_store_dirty = false;
        Ok(())
    }

    /// Writes the full rewrite runtime state back to stable memory using the facade-level result type.
    pub fn try_write_all_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<()> {
        self.write_all_to_stable_memory(memory)
    }

    /// Refreshes dirty label sidecars and writes only dirty regions back to stable memory.
    pub fn refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<(Vec<usize>, Vec<usize>)> {
        let property_store_was_dirty =
            self.node_property_store_dirty || self.edge_property_store_dirty;
        let refreshed = self
            .graph
            .refresh_and_write_dirty_to_stable_memory(&mut self.manager, memory)?;
        if self.node_property_store_dirty {
            write_graph_property_store_to_stable_memory(
                &mut self.manager,
                memory,
                RegionKind::NodePropertyStore,
                &self.node_property_store,
            )?;
            self.node_property_store_dirty = false;
        }
        if self.edge_property_store_dirty {
            write_graph_property_store_to_stable_memory(
                &mut self.manager,
                memory,
                RegionKind::EdgePropertyStore,
                &self.edge_property_store,
            )?;
            self.edge_property_store_dirty = false;
        }
        if !property_store_was_dirty {
            return Ok(refreshed);
        }
        self.sync_property_index_node_stores();
        let image = self.compact_property_index_storage_image();
        write_property_index_storage_image_to_stable_memory(&mut self.manager, memory, &image)?;
        Ok(refreshed)
    }

    /// Refreshes dirty state and writes it back using the facade-level result type.
    pub fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<(Vec<usize>, Vec<usize>)> {
        self.refresh_and_write_dirty_to_stable_memory(memory)
    }

    /// Appends one empty vertex slot pair to the forward and reverse surfaces.
    pub fn append_empty_vertex_pair(&mut self) -> RewriteGraphPmaResult<(usize, usize)> {
        self.graph
            .append_empty_vertex_pair()
            .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)
    }

    /// Appends `count` empty vertex slot pairs to the forward and reverse surfaces.
    pub fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> RewriteGraphPmaResult<Vec<(usize, usize)>> {
        self.graph
            .append_empty_vertex_pairs(count)
            .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)
    }

    /// Appends one empty vertex slot pair and writes back resulting dirty state.
    pub fn append_empty_vertex_pair_and_write(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteAppendVertexWriteSummary> {
        let ordinals = self.append_empty_vertex_pair()?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewriteAppendVertexWriteSummary {
            ordinals,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::AppendVertex(summary.clone()));
        Ok(summary)
    }

    /// Appends `count` empty vertex slot pairs and writes back resulting dirty state.
    pub fn append_empty_vertex_pairs_and_write(
        &mut self,
        count: usize,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteAppendVerticesWriteSummary> {
        let ordinals = self.append_empty_vertex_pairs(count)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewriteAppendVerticesWriteSummary {
            ordinals,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::AppendVertices(summary.clone()));
        Ok(summary)
    }

    /// Appends two new empty vertex slots and inserts one first edge between them.
    ///
    /// This is the smallest adjacency-side bootstrap step after
    /// `bootstrap_empty(...)`: it creates source/destination ordinals and then
    /// inserts one logical edge without requiring the caller to wire those
    /// steps together manually.
    pub fn bootstrap_edge_between_new_vertices_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        dst_vertex: NodeId,
        label_id: LabelId,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteBootstrapEdgeWriteSummary> {
        let (src_ordinal, _) = self.append_empty_vertex_pair()?;
        let (dst_ordinal, _) = self.append_empty_vertex_pair()?;
        let insert = self
            .graph
            .insert_edge_pair(
                edge_id,
                src_vertex,
                src_ordinal,
                dst_vertex,
                dst_ordinal,
                label_id,
            )
            .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewriteBootstrapEdgeWriteSummary {
            ordinals: (src_ordinal, dst_ordinal),
            insert,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::BootstrapEdge(summary.clone()));
        Ok(summary)
    }

    /// Appends new vertex slots for `vertex_ids`, inserts the supplied initial edges,
    /// then writes the resulting dirty state.
    ///
    /// `initial_edges` refers to vertices by their position inside `vertex_ids`.
    pub fn bootstrap_vertices_and_edges_and_write(
        &mut self,
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        let ordinals = self.append_empty_vertex_pairs(vertex_ids.len())?;
        let vertex_ordinals: Vec<RewriteVertexOrdinalMapping> = vertex_ids
            .iter()
            .copied()
            .zip(ordinals.iter().copied())
            .map(
                |(vertex_id, (forward_ordinal, reverse_ordinal))| RewriteVertexOrdinalMapping {
                    vertex_id,
                    forward_ordinal,
                    reverse_ordinal,
                },
            )
            .collect();

        let mut inserts = Vec::with_capacity(initial_edges.len());
        let mut locators = Vec::with_capacity(initial_edges.len());
        for (edge_id, src_index, dst_index, label_id) in initial_edges.iter().copied() {
            let Some(src_mapping) = vertex_ordinals.get(src_index).copied() else {
                return Err(RewriteGraphPmaError::InvalidLocatorInputs);
            };
            let Some(dst_mapping) = vertex_ordinals.get(dst_index).copied() else {
                return Err(RewriteGraphPmaError::InvalidLocatorInputs);
            };
            let insert = self
                .graph
                .insert_edge_pair(
                    edge_id,
                    src_mapping.vertex_id,
                    src_mapping.forward_ordinal,
                    dst_mapping.vertex_id,
                    dst_mapping.reverse_ordinal,
                    label_id,
                )
                .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)?;
            let GraphInsertResult::Inserted {
                locators: (forward, reverse),
                ..
            } = insert
            else {
                return Err(RewriteGraphPmaError::InvalidLocatorInputs);
            };
            inserts.push(insert);
            locators.push(RewriteEdgeLocatorMapping {
                edge_id,
                canonical: forward,
                forward,
                reverse,
            });
        }

        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewriteBootstrapGraphWriteSummary {
            vertex_ordinals,
            inserts,
            locators,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::BootstrapGraph(summary.clone()));
        Ok(summary)
    }

    fn define_empty_surface_regions(
        manager: &mut RegionManager,
        surface: crate::low_level::SurfaceKind,
    ) {
        let kinds = match surface {
            crate::low_level::SurfaceKind::Forward => [
                RegionKind::ForwardVertexTable,
                RegionKind::ForwardEdgeEntries,
                RegionKind::ForwardLabelIndex,
                RegionKind::ForwardSegmentLog,
            ],
            crate::low_level::SurfaceKind::Reverse => [
                RegionKind::ReverseVertexTable,
                RegionKind::ReverseEdgeEntries,
                RegionKind::ReverseLabelIndex,
                RegionKind::ReverseSegmentLog,
            ],
        };

        for kind in kinds {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    0,
                    WasmPages::new(1),
                    WasmPages::new(1),
                ),
            );
        }
    }

    /// Defines empty fixed-size property-store regions.
    fn define_empty_property_regions(manager: &mut RegionManager) {
        for kind in [
            RegionKind::NodePropertyStore,
            RegionKind::EdgePropertyStore,
            RegionKind::PropertyIndex,
        ] {
            manager.define_bucket_region(kind, default_property_region_chain());
        }
    }

    /// Ensures local capacity for an upcoming batch and writes back any dirty state.
    pub fn ensure_local_capacity_for_incoming_live_entries_and_write(
        &mut self,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        planned_incoming_live_entries: u32,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
    ) -> Result<GraphEnsureCapacityWriteSummary, WritebackError> {
        let summary = self
            .graph
            .ensure_local_capacity_for_incoming_live_entries_and_write(
                src_vertex,
                src_ordinal,
                dst_vertex,
                dst_ordinal,
                planned_incoming_live_entries,
                forward_rebalance_vertex_ids,
                forward_base_edge_ids_by_ordinal,
                &mut self.manager,
                memory,
            )?;
        self.record_write_event(RewriteFacadeWriteEvent::EnsureCapacity(summary.clone()));
        Ok(summary)
    }

    /// Inserts one logical edge, performing one local rebalance cycle first if needed,
    /// then writes back dirty state.
    pub fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        let summary = self.graph.insert_edge_pair_with_local_rebalance_and_write(
            edge_id,
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            label_id,
            forward_rebalance_vertex_ids,
            forward_base_edge_ids_by_ordinal,
            &mut self.manager,
            memory,
        )?;
        self.record_write_event(RewriteFacadeWriteEvent::InsertEdge(summary.clone()));
        Ok(summary)
    }

    /// Replaces one logical edge and then writes back any dirty state.
    pub fn replace_edge_pair_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
        memory: &impl Memory,
    ) -> Result<
        RewriteGraphMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>,
        WritebackError,
    > {
        let mutation = self
            .graph
            .replace_edge_pair(
                edge_id,
                src_vertex,
                src_ordinal,
                src_logical_index,
                dst_vertex,
                dst_ordinal,
                dst_logical_index,
                label_id,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                crate::low_level::RegionKind::ForwardEdgeEntries,
            ))?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) = self
            .graph
            .refresh_and_write_dirty_to_stable_memory(&mut self.manager, memory)?;
        let summary = RewriteGraphMutationWriteSummary {
            mutation,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::ReplaceEdge(summary.clone()));
        Ok(summary)
    }

    /// Tombstones one logical edge and then writes back any dirty state.
    pub fn tombstone_edge_pair_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        memory: &impl Memory,
    ) -> Result<RewriteGraphMutationWriteSummary<GraphMutationPath>, WritebackError> {
        let mutation = self
            .graph
            .tombstone_edge_pair(
                edge_id,
                src_vertex,
                src_ordinal,
                src_logical_index,
                dst_vertex,
                dst_ordinal,
                dst_logical_index,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                crate::low_level::RegionKind::ForwardEdgeEntries,
            ))?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) = self
            .graph
            .refresh_and_write_dirty_to_stable_memory(&mut self.manager, memory)?;
        let summary = RewriteGraphMutationWriteSummary {
            mutation,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::DeleteEdge(summary.clone()));
        Ok(summary)
    }

    /// Starts one facade-level batch mutation session.
    pub fn begin_batch_mutation<'a, M: Memory>(
        &'a mut self,
        memory: &'a M,
    ) -> RewriteGraphPmaBatchSession<'a, M> {
        RewriteGraphPmaBatchSession::new(&mut self.graph, &mut self.manager, memory)
    }

    /// Binds this facade together with one stable-memory handle behind a thin adapter.
    pub fn bind<'a, M: Memory>(
        &'a mut self,
        memory: &'a M,
    ) -> RewriteGraphStoreAdapter<'a, Self, M> {
        RewriteGraphStoreAdapter::new(self, memory)
    }

    /// Binds this facade and immediately exposes the kernel-facing overlay graph.
    pub fn bind_kernel_overlay<'a, M: Memory>(
        &'a mut self,
        memory: &'a M,
    ) -> RewriteGraphPmaKernelOverlay<'a, M> {
        self.bind(memory).into_kernel_overlay()
    }
}

impl RewriteGraphStore for RewriteGraphPma {
    fn last_write_event(&self) -> Option<&RewriteFacadeWriteEvent> {
        Self::last_write_event(self)
    }

    fn write_history(&self) -> &[RewriteFacadeWriteEvent] {
        Self::write_history(self)
    }

    fn manager(&self) -> &RegionManager {
        Self::manager(self)
    }

    fn manager_mut(&mut self) -> &mut RegionManager {
        Self::manager_mut(self)
    }

    fn graph(&self) -> &GraphRuntime {
        Self::graph(self)
    }

    fn graph_mut(&mut self) -> &mut GraphRuntime {
        Self::graph_mut(self)
    }

    fn node_property_store(&self) -> &GraphPropertyAppendLog {
        Self::node_property_store(self)
    }

    fn node_property_store_mut(&mut self) -> &mut GraphPropertyAppendLog {
        Self::node_property_store_mut(self)
    }

    fn edge_property_store(&self) -> &GraphPropertyAppendLog {
        Self::edge_property_store(self)
    }

    fn edge_property_store_mut(&mut self) -> &mut GraphPropertyAppendLog {
        Self::edge_property_store_mut(self)
    }

    fn scan_node_properties(&self, node_id: NodeId) -> PropertyMap {
        Self::scan_node_properties(self, node_id)
    }

    fn scan_edge_properties(&self, edge_id: EdgeId) -> PropertyMap {
        Self::scan_edge_properties(self, edge_id)
    }

    fn scan_node_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<NodeId> {
        Self::scan_node_ids_by_property_eq(self, property, value)
    }

    fn scan_node_ids_by_property(&self, property: &str) -> Vec<NodeId> {
        Self::scan_node_ids_by_property(self, property)
    }

    fn scan_edge_ids_by_property(&self, property: &str) -> Vec<EdgeId> {
        Self::scan_edge_ids_by_property(self, property)
    }

    fn scan_edge_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<EdgeId> {
        Self::scan_edge_ids_by_property_eq(self, property, value)
    }

    fn node_property_store_is_dirty(&self) -> bool {
        Self::node_property_store_is_dirty(self)
    }

    fn edge_property_store_is_dirty(&self) -> bool {
        Self::edge_property_store_is_dirty(self)
    }

    fn set_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        Self::set_node_property_value(self, node_id, property, value)
    }

    fn remove_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        Self::remove_node_property_value(self, node_id, property)
    }

    fn set_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        Self::set_edge_property_value(self, edge_id, property, value)
    }

    fn remove_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        Self::remove_edge_property_value(self, edge_id, property)
    }

    fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        Self::set_node_property_value_and_write(self, node_id, property, value, memory)
    }

    fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        Self::remove_node_property_value_and_write(self, node_id, property, memory)
    }

    fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        Self::set_edge_property_value_and_write(self, edge_id, property, value, memory)
    }

    fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        Self::remove_edge_property_value_and_write(self, edge_id, property, memory)
    }

    fn try_rebuild_locator_sidecar(
        &mut self,
        forward_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> RewriteGraphPmaResult<()> {
        Self::try_rebuild_locator_sidecar(
            self,
            forward_vertex_ids,
            forward_base_edge_ids_by_ordinal,
        )
    }

    fn try_write_all_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<()> {
        Self::try_write_all_to_stable_memory(self, memory)
    }

    fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<(Vec<usize>, Vec<usize>)> {
        Self::try_refresh_and_write_dirty_to_stable_memory(self, memory)
    }

    fn append_empty_vertex_pair(&mut self) -> RewriteGraphPmaResult<(usize, usize)> {
        Self::append_empty_vertex_pair(self)
    }

    fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> RewriteGraphPmaResult<Vec<(usize, usize)>> {
        Self::append_empty_vertex_pairs(self, count)
    }

    fn bootstrap_vertices_and_edges_and_write(
        &mut self,
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        Self::bootstrap_vertices_and_edges_and_write(self, vertex_ids, initial_edges, memory)
    }

    fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        Self::insert_edge_pair_with_local_rebalance_and_write(
            self,
            edge_id,
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            label_id,
            forward_rebalance_vertex_ids,
            forward_base_edge_ids_by_ordinal,
            memory,
        )
    }

    fn replace_edge_pair_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
        memory: &impl Memory,
    ) -> Result<
        RewriteGraphMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>,
        WritebackError,
    > {
        Self::replace_edge_pair_and_write(
            self,
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
            label_id,
            memory,
        )
    }

    fn tombstone_edge_pair_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        memory: &impl Memory,
    ) -> Result<RewriteGraphMutationWriteSummary<GraphMutationPath>, WritebackError> {
        Self::tombstone_edge_pair_and_write(
            self,
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
            memory,
        )
    }
}

impl<'a, M: Memory> RewriteGraphPmaBatchSession<'a, M> {
    /// Creates one facade-level batch mutation session.
    pub fn new(graph: &'a mut GraphRuntime, manager: &'a mut RegionManager, memory: &'a M) -> Self {
        Self {
            inner: GraphBatchMutationSession::new(graph, manager, memory),
        }
    }

    /// Returns the graph runtime currently being mutated.
    pub fn graph(&self) -> &GraphRuntime {
        self.inner.graph()
    }

    /// Returns the graph runtime mutably.
    pub fn graph_mut(&mut self) -> &mut GraphRuntime {
        self.inner.graph_mut()
    }

    /// Prepares local capacity for an upcoming batch without inserting yet.
    pub fn prepare_local_capacity(
        &mut self,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        planned_incoming_live_entries: u32,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<bool> {
        self.inner.prepare_local_capacity(
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            planned_incoming_live_entries,
            forward_rebalance_vertex_ids,
            forward_base_edge_ids_by_ordinal,
        )
    }

    /// Inserts one edge using the batch-aware rebalance path without flushing yet.
    pub fn insert_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        planned_incoming_live_entries: u32,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<GraphInsertResult> {
        self.inner.insert_edge_pair(
            edge_id,
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            label_id,
            planned_incoming_live_entries,
            forward_rebalance_vertex_ids,
            forward_base_edge_ids_by_ordinal,
        )
    }

    /// Replaces one logical edge without flushing yet.
    pub fn replace_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
    ) -> Option<(GraphMutationPath, (EdgeEntry, EdgeEntry))> {
        self.inner.replace_edge_pair(
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
            label_id,
        )
    }

    /// Tombstones one logical edge without flushing yet.
    pub fn tombstone_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
    ) -> Option<GraphMutationPath> {
        self.inner.tombstone_edge_pair(
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
        )
    }

    /// Flushes dirty graph state accumulated so far in this batch.
    pub fn flush(&mut self) -> Result<(Vec<usize>, Vec<usize>), WritebackError> {
        self.inner.flush()
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> RewriteGraphStoreAdapter<'a, S, M> {
    /// Creates one adapter over a rewrite store plus stable memory.
    pub fn new(store: &'a mut S, memory: &'a M) -> Self {
        Self { store, memory }
    }

    /// Returns immutable access to the wrapped rewrite store.
    pub fn store(&self) -> &S {
        self.store
    }

    /// Returns the most recent facade-level write event observed through the bound store.
    pub fn last_write_event(&self) -> Option<&RewriteFacadeWriteEvent> {
        self.store.last_write_event()
    }

    /// Returns recent facade-level write events in observation order.
    pub fn write_history(&self) -> &[RewriteFacadeWriteEvent] {
        self.store.write_history()
    }

    /// Returns the recent façade write history projected onto the shared event vocabulary.
    pub fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection> {
        self.write_history()
            .iter()
            .flat_map(RewriteFacadeWriteEvent::shared_projections)
            .collect()
    }

    /// Returns the recent bound-store write history formatted as compact diagnostics lines.
    pub fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(&self.shared_write_history())
    }

    pub fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(&self.shared_write_history())
    }

    /// Returns mutable access to the wrapped rewrite store.
    pub fn store_mut(&mut self) -> &mut S {
        self.store
    }

    /// Consumes the adapter and returns its wrapped store plus bound memory.
    pub fn into_parts(self) -> (&'a mut S, &'a M) {
        (self.store, self.memory)
    }

    /// Appends one empty vertex slot pair.
    pub fn append_empty_vertex_pair(&mut self) -> RewriteGraphPmaResult<(usize, usize)> {
        self.store.append_empty_vertex_pair()
    }

    /// Appends `count` empty vertex slot pairs.
    pub fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> RewriteGraphPmaResult<Vec<(usize, usize)>> {
        self.store.append_empty_vertex_pairs(count)
    }

    /// Bootstraps multiple vertices and initial edges using the bound memory handle.
    pub fn bootstrap_vertices_and_edges(
        &mut self,
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        self.store
            .bootstrap_vertices_and_edges_and_write(vertex_ids, initial_edges, self.memory)
    }

    /// Inserts one logical edge using the bound memory handle.
    pub fn insert_edge_pair_with_local_rebalance(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        self.store.insert_edge_pair_with_local_rebalance_and_write(
            edge_id,
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            label_id,
            forward_rebalance_vertex_ids,
            forward_base_edge_ids_by_ordinal,
            self.memory,
        )
    }

    /// Replaces one logical edge using the bound memory handle.
    pub fn replace_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
    ) -> Result<
        RewriteGraphMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>,
        WritebackError,
    > {
        self.store.replace_edge_pair_and_write(
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
            label_id,
            self.memory,
        )
    }

    /// Tombstones one logical edge using the bound memory handle.
    pub fn tombstone_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
    ) -> Result<RewriteGraphMutationWriteSummary<GraphMutationPath>, WritebackError> {
        self.store.tombstone_edge_pair_and_write(
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
            self.memory,
        )
    }

    /// Flushes dirty state using the bound memory handle.
    pub fn flush_dirty(&mut self) -> RewriteGraphPmaResult<RewriteRefreshedVertices> {
        let (forward, reverse) = self
            .store
            .try_refresh_and_write_dirty_to_stable_memory(self.memory)?;
        Ok(RewriteRefreshedVertices::new(forward, reverse))
    }

    /// Resolves one forward-surface locator against the current rewrite runtime.
    pub fn resolve_forward_edge_slot(
        &self,
        vertex: NodeId,
        ordinal: usize,
        locator: EdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.store
            .graph()
            .forward
            .resolve_edge_slot(vertex, ordinal, locator)
    }

    /// Resolves one reverse-surface locator against the current rewrite runtime.
    pub fn resolve_reverse_edge_slot(
        &self,
        vertex: NodeId,
        ordinal: usize,
        locator: EdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.store
            .graph()
            .reverse
            .resolve_edge_slot(vertex, ordinal, locator)
    }
}

impl<'a, M: Memory> RewriteGraphStoreAdapter<'a, RewriteGraphPma, M> {
    /// Starts one facade-level batch mutation session through the bound adapter.
    pub fn begin_batch_mutation(&'a mut self) -> RewriteGraphPmaBatchSession<'a, M> {
        self.store.begin_batch_mutation(self.memory)
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> RewriteGraphService
    for RewriteGraphStoreAdapter<'a, S, M>
{
    fn last_write_event(&self) -> Option<&RewriteFacadeWriteEvent> {
        Self::last_write_event(self)
    }

    fn write_history(&self) -> &[RewriteFacadeWriteEvent] {
        Self::write_history(self)
    }

    fn bootstrap_vertices_and_edges(
        &mut self,
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        Self::bootstrap_vertices_and_edges(self, vertex_ids, initial_edges)
    }

    fn insert_edge_pair_with_local_rebalance(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        Self::insert_edge_pair_with_local_rebalance(
            self,
            edge_id,
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            label_id,
            forward_rebalance_vertex_ids,
            forward_base_edge_ids_by_ordinal,
        )
    }

    fn replace_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
    ) -> Result<
        RewriteGraphMutationWriteSummary<(GraphMutationPath, (EdgeEntry, EdgeEntry))>,
        WritebackError,
    > {
        Self::replace_edge_pair(
            self,
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
            label_id,
        )
    }

    fn tombstone_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
    ) -> Result<RewriteGraphMutationWriteSummary<GraphMutationPath>, WritebackError> {
        Self::tombstone_edge_pair(
            self,
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
        )
    }

    fn flush_dirty(&mut self) -> RewriteGraphPmaResult<RewriteRefreshedVertices> {
        Self::flush_dirty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RewriteAppendVertexWriteSummary, RewriteAppendVerticesWriteSummary,
        RewriteBootstrapEdgeProjection, RewriteBootstrapEdgeWriteSummary,
        RewriteBootstrapGraphProjection, RewriteBootstrapGraphWriteSummary,
        RewriteBootstrapVerticesProjection, RewriteEdgeLocatorMapping, RewriteEdgeWriteOperation,
        RewriteEnsureCapacityProjection, RewriteFacadeWriteEvent, RewriteGraphMutationWriteSummary,
        RewriteGraphPma, RewriteGraphPmaError, RewriteGraphPmaResult, RewriteGraphService,
        RewriteGraphStore, RewriteGraphStoreAdapter, RewriteInsertEdgeProjection,
        RewritePropertyIndexTouchedSections, RewritePropertyWriteProjection,
        RewriteRefreshedVertices, RewriteVertexOrdinalMapping, RewriteWriteEventProjection,
    };
    use crate::low_level::{
        encode_edge_entries, encode_label_index_region, encode_overflow_entries,
        encode_vertex_entries, BucketSizeInPages, EdgeEntry, EdgeIndex, EdgeMeta, RegionKind,
        RegionManager, VertexEntry,
    };
    use crate::observability::{project_facade_write_event, project_facade_write_history};
    use crate::property_index::PropertyIndexNodeStoreMutationKind;
    use crate::property_store::{
        write_graph_property_store_to_stable_memory, GraphPropertyAppendLog, PropertyKey,
    };
    use crate::stable::{Memory, VecMemory};
    use crate::{
        read_property_index_snapshot_section_from_stable_memory,
        write_property_index_storage_image_to_stable_memory, PropertyIndexEntry, PropertyIndexKey,
        PropertyIndexNodeHeader, PropertyIndexNodeRecord, PropertyIndexSnapshot,
        PropertyIndexStorageImage,
    };
    use crate::{GraphInsertResult, GraphMutationPath};
    use gleaph_gql::Value;
    use gleaph_graph_kernel::NodeId;

    fn assert_projected_history(
        events: &[RewriteFacadeWriteEvent],
        expected: Vec<RewriteWriteEventProjection>,
    ) {
        assert_eq!(project_facade_write_history(events), expected);
    }

    fn define_surface_regions(manager: &mut RegionManager, prefix: crate::low_level::SurfaceKind) {
        RewriteGraphPma::define_empty_surface_regions(manager, prefix);
    }

    fn define_property_regions(manager: &mut RegionManager) {
        RewriteGraphPma::define_empty_property_regions(manager);
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
        write_graph_property_store_to_stable_memory(
            &mut manager,
            &memory,
            RegionKind::NodePropertyStore,
            &GraphPropertyAppendLog::default(),
        )
        .unwrap();
        write_graph_property_store_to_stable_memory(
            &mut manager,
            &memory,
            RegionKind::EdgePropertyStore,
            &GraphPropertyAppendLog::default(),
        )
        .unwrap();

        (manager, memory)
    }

    #[test]
    fn facade_hydrates_from_stable_memory() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = RewriteGraphPma::hydrate_from_stable_memory(manager, &memory).unwrap();

        assert_eq!(facade.graph.forward.0.vertices.len(), 1);
        assert_eq!(facade.graph.forward.0.base_entries.len(), 1);
        assert_eq!(facade.graph.reverse.0.vertices.len(), 1);
        assert_eq!(facade.graph.reverse.0.base_entries.len(), 1);
    }

    #[test]
    fn facade_refresh_and_write_dirty_round_trips() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = RewriteGraphPma::hydrate_from_stable_memory(manager, &memory).unwrap();

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
        let _ = facade
            .refresh_and_write_dirty_to_stable_memory(&memory)
            .unwrap();

        let rehydrated =
            RewriteGraphPma::hydrate_from_stable_memory(facade.manager.clone(), &memory).unwrap();
        assert_eq!(
            rehydrated.graph.forward.0.base_entries[0],
            EdgeEntry::new(dst, EdgeMeta::new(9, false))
        );
        assert_eq!(
            rehydrated.graph.reverse.0.base_entries[0],
            EdgeEntry::new(src, EdgeMeta::new(9, false))
        );
    }

    #[test]
    fn facade_property_stores_round_trip_through_stable_memory() {
        let memory = VecMemory::default();
        let mut facade =
            RewriteGraphPma::bootstrap_empty_with_bucket_size(BucketSizeInPages::new(1), &memory)
                .expect("bootstrap");
        let node_id = NodeId::from(11u8);

        facade
            .node_property_store_mut()
            .set(
                PropertyKey::node(node_id, "profile"),
                Value::Text("x".repeat((crate::low_level::WASM_PAGE_SIZE as usize) + 512)),
            )
            .expect("set node property");
        facade
            .edge_property_store_mut()
            .set(PropertyKey::edge(77, "weight"), Value::Int64(9))
            .expect("set edge property");
        facade
            .try_write_all_to_stable_memory(&memory)
            .expect("write all");

        let rehydrated =
            RewriteGraphPma::hydrate_from_stable_memory(facade.manager.clone(), &memory).unwrap();
        assert_eq!(
            rehydrated
                .node_property_store()
                .get_node_property(node_id, "profile"),
            Some(Value::Text(
                "x".repeat((crate::low_level::WASM_PAGE_SIZE as usize) + 512)
            ))
        );
        assert_eq!(
            rehydrated
                .edge_property_store()
                .get_edge_property(77, "weight"),
            Some(Value::Int64(9))
        );
    }

    #[test]
    fn facade_property_store_dirty_write_round_trips_and_clears_dirty_flags() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let node_id = NodeId::from(21u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("u21".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(901, "weight", &Value::Int64(42))
            .expect("set edge property");
        assert!(facade.node_property_store_dirty);
        assert!(facade.edge_property_store_dirty);

        let refreshed = facade
            .refresh_and_write_dirty_to_stable_memory(&memory)
            .expect("write dirty");
        assert_eq!(refreshed, (Vec::new(), Vec::new()));
        assert!(!facade.node_property_store_dirty);
        assert!(!facade.edge_property_store_dirty);

        let rehydrated =
            RewriteGraphPma::hydrate_from_stable_memory(facade.manager.clone(), &memory).unwrap();
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
    fn facade_property_index_tracks_equality_updates_and_removals() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
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
        assert!(facade
            .scan_node_ids_by_property_eq("uid", &Value::Text("alice".into()))
            .is_empty());
        assert_eq!(
            facade.scan_node_ids_by_property_eq("uid", &Value::Text("bob".into())),
            vec![node_id]
        );

        facade
            .remove_node_property_value(node_id, "uid")
            .expect("remove property");
        assert!(facade
            .scan_node_ids_by_property_eq("uid", &Value::Text("bob".into()))
            .is_empty());
    }

    #[test]
    fn facade_property_index_mutation_summary_reports_touched_nodes() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
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
            vec![PropertyIndexNodeStoreMutationKind::Rebuild]
        );
        assert!(!first.touched_node_ids.is_empty());
        assert!(!first.allocated_node_ids.is_empty());
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
                PropertyIndexNodeStoreMutationKind::Rebuild,
            ]
        );
        assert!(!overwrite.touched_node_ids.is_empty());

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
        assert!(!removal.touched_node_ids.is_empty());
        assert!(
            !removal.freed_node_ids.is_empty()
                || !removal.allocated_node_ids.is_empty()
                || !removal.touched_node_ids.is_empty()
        );
    }

    #[test]
    fn facade_property_index_mutation_summary_reports_leaf_redistribution_without_allocations() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");

        let left = facade
            .node_property_index_nodes
            .allocate(PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    2,
                    crate::PropertyIndexNodeId::NULL,
                    crate::PropertyIndexNodeId(2),
                ),
                entries: vec![
                    (
                        PropertyIndexKey::node(
                            NodeId::from(1u8),
                            "uid",
                            Value::Text("alice".into())
                                .to_stable_bytes()
                                .expect("stable bytes"),
                        ),
                        PropertyIndexEntry::empty(),
                    ),
                    (
                        PropertyIndexKey::node(
                            NodeId::from(2u8),
                            "uid",
                            Value::Text("bob".into())
                                .to_stable_bytes()
                                .expect("stable bytes"),
                        ),
                        PropertyIndexEntry::empty(),
                    ),
                ],
            });
        let right = facade
            .node_property_index_nodes
            .allocate(PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(1, left, crate::PropertyIndexNodeId::NULL),
                entries: vec![(
                    PropertyIndexKey::node(
                        NodeId::from(4u8),
                        "uid",
                        Value::Text("dave".into())
                            .to_stable_bytes()
                            .expect("stable bytes"),
                    ),
                    PropertyIndexEntry::empty(),
                )],
            });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) =
            facade.node_property_index_nodes.get_mut(left)
        {
            header.next_leaf = right;
        }
        let root = facade
            .node_property_index_nodes
            .allocate(PropertyIndexNodeRecord::Internal {
                header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
                keys: vec![PropertyIndexKey::node(
                    NodeId::from(4u8),
                    "uid",
                    Value::Text("dave".into())
                        .to_stable_bytes()
                        .expect("stable bytes"),
                )],
                children: vec![left, right],
            });
        facade.node_property_index = facade.node_property_index_nodes.to_index(64);
        assert_eq!(facade.node_property_index.header.root, root);

        let summary = facade
            .set_node_property_value_with_summary(
                NodeId::from(3u8),
                "uid",
                &Value::Text("carol".into()),
            )
            .expect("set property with redistribution");

        assert_eq!(
            summary.sections,
            RewritePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert_eq!(
            summary.node_store_operations,
            vec![PropertyIndexNodeStoreMutationKind::LocalUpdate]
        );
        assert_eq!(summary.allocated_node_ids, Vec::new());
        assert_eq!(summary.freed_node_ids, Vec::new());
        assert_eq!(summary.touched_node_ids, vec![right]);
        assert_eq!(
            crate::observability::format_write_event_projection(
                &RewriteWriteEventProjection::Property(RewritePropertyWriteProjection {
                    sections: summary.sections,
                    node_store_operations: summary.node_store_operations.clone(),
                    touched_node_ids: summary.touched_node_ids.clone(),
                    allocated_node_ids: summary.allocated_node_ids.clone(),
                    freed_node_ids: summary.freed_node_ids.clone(),
                    flushed_sections: summary.sections,
                    refreshed: RewriteRefreshedVertices::new(Vec::new(), Vec::new()),
                })
            ),
            "property sections=(true,true,true) ops=local-update nodes=touched:1 [2] alloc:0 [] freed:0 [] flushed=(true,true,true) refreshed=(0,0) fwd=[] rev=[]"
        );
        assert_eq!(
            facade.scan_node_ids_by_property_eq("uid", &Value::Text("carol".into())),
            vec![NodeId::from(3u8)]
        );
    }

    #[test]
    fn facade_property_index_mutation_summary_reports_edge_updates() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");

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
    fn facade_edge_property_index_mutation_summary_reports_leaf_redistribution_without_allocations()
    {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");

        let left = facade
            .edge_property_index_nodes
            .allocate(PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    2,
                    crate::PropertyIndexNodeId::NULL,
                    crate::PropertyIndexNodeId(2),
                ),
                entries: vec![
                    (
                        PropertyIndexKey::edge(
                            701,
                            "weight",
                            Value::Int64(1).to_stable_bytes().expect("stable bytes"),
                        ),
                        PropertyIndexEntry::empty(),
                    ),
                    (
                        PropertyIndexKey::edge(
                            702,
                            "weight",
                            Value::Int64(2).to_stable_bytes().expect("stable bytes"),
                        ),
                        PropertyIndexEntry::empty(),
                    ),
                ],
            });
        let right = facade
            .edge_property_index_nodes
            .allocate(PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(1, left, crate::PropertyIndexNodeId::NULL),
                entries: vec![(
                    PropertyIndexKey::edge(
                        704,
                        "weight",
                        Value::Int64(4).to_stable_bytes().expect("stable bytes"),
                    ),
                    PropertyIndexEntry::empty(),
                )],
            });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) =
            facade.edge_property_index_nodes.get_mut(left)
        {
            header.next_leaf = right;
        }
        let root = facade
            .edge_property_index_nodes
            .allocate(PropertyIndexNodeRecord::Internal {
                header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
                keys: vec![PropertyIndexKey::edge(
                    704,
                    "weight",
                    Value::Int64(4).to_stable_bytes().expect("stable bytes"),
                )],
                children: vec![left, right],
            });
        facade.edge_property_index = facade.edge_property_index_nodes.to_index(64);
        assert_eq!(facade.edge_property_index.header.root, root);

        let summary = facade
            .set_edge_property_value_with_summary(703, "weight", &Value::Int64(3))
            .expect("set edge property with redistribution");

        assert_eq!(
            summary.sections,
            RewritePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store: true,
            }
        );
        assert_eq!(
            summary.node_store_operations,
            vec![PropertyIndexNodeStoreMutationKind::Redistribute]
        );
        assert_eq!(summary.allocated_node_ids, Vec::new());
        assert_eq!(summary.freed_node_ids, Vec::new());
        assert_eq!(summary.touched_node_ids, vec![right, root]);
        assert_eq!(
            crate::observability::format_write_event_projection(
                &RewriteWriteEventProjection::Property(RewritePropertyWriteProjection {
                    sections: summary.sections,
                    node_store_operations: summary.node_store_operations.clone(),
                    touched_node_ids: summary.touched_node_ids.clone(),
                    allocated_node_ids: summary.allocated_node_ids.clone(),
                    freed_node_ids: summary.freed_node_ids.clone(),
                    flushed_sections: summary.sections,
                    refreshed: RewriteRefreshedVertices::new(Vec::new(), Vec::new()),
                })
            ),
            "property sections=(true,true,true) ops=redistribute nodes=touched:2 [2,3] alloc:0 [] freed:0 [] flushed=(true,true,true) refreshed=(0,0) fwd=[] rev=[]"
        );
        assert_eq!(
            facade.scan_edge_ids_by_property_eq("weight", &Value::Int64(3)),
            vec![703]
        );
    }

    #[test]
    fn facade_property_mutation_write_summary_flushes_and_round_trips() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let node_id = NodeId::from(33u8);

        let summary = facade
            .set_node_property_value_and_write(
                node_id,
                "uid",
                &Value::Text("carol".into()),
                &memory,
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
        assert!(facade
            .scan_node_ids_by_property_eq_preferring_stable_memory(
                &memory,
                "uid",
                &Value::Text("carol".into()),
            )
            .contains(&node_id));

        let rehydrated =
            RewriteGraphPma::hydrate_from_stable_memory(facade.manager.clone(), &memory).unwrap();
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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");

        let set = facade
            .set_edge_property_value_and_write(702, "weight", &Value::Int64(9), &memory)
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
            .remove_edge_property_value_and_write(702, "weight", &memory)
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
        assert!(facade
            .scan_edge_ids_by_property_eq_preferring_stable_memory(
                &memory,
                "weight",
                &Value::Int64(9),
            )
            .is_empty());
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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let src = NodeId::from(81u8);
        let dst = NodeId::from(82u8);
        let bootstrap = facade
            .bootstrap_vertices_and_edges_and_write(&[src, dst], &[(9001, 0, 1, 7)], &memory)
            .expect("bootstrap graph");
        let src_ordinal = bootstrap.vertex_ordinals[0].forward_ordinal;
        let dst_ordinal = bootstrap.vertex_ordinals[1].reverse_ordinal;

        let replace = facade
            .replace_edge_pair_and_write(9001, src, src_ordinal, 0, dst, dst_ordinal, 0, 9, &memory)
            .expect("replace edge");
        assert_eq!(replace.mutation.0, GraphMutationPath::Base);

        let delete = facade
            .tombstone_edge_pair_and_write(9001, src, src_ordinal, 0, dst, dst_ordinal, 0, &memory)
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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let node_id = NodeId::from(41u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(777, "weight", &Value::Int64(5))
            .expect("set edge property");
        facade
            .try_write_all_to_stable_memory(&memory)
            .expect("write all");

        let rehydrated =
            RewriteGraphPma::hydrate_from_stable_memory(facade.manager.clone(), &memory).unwrap();
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
    fn facade_writes_compact_property_index_snapshot_when_node_store_is_present() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let node_id = NodeId::from(42u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(778, "weight", &Value::Int64(5))
            .expect("set edge property");
        facade
            .try_write_all_to_stable_memory(&memory)
            .expect("write all");

        let snapshot =
            read_property_index_snapshot_section_from_stable_memory(&facade.manager, &memory)
                .expect("read compact snapshot section");
        assert_eq!(snapshot, PropertyIndexSnapshot::empty(64));

        let rehydrated =
            RewriteGraphPma::hydrate_from_stable_memory(facade.manager.clone(), &memory).unwrap();
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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let src = NodeId::from(91u8);
        let dst = NodeId::from(92u8);

        let bootstrap = facade
            .bootstrap_vertices_and_edges_and_write(&[src, dst], &[], &memory)
            .expect("bootstrap vertices");
        let src_mapping = &bootstrap.vertex_ordinals[0];
        let dst_mapping = &bootstrap.vertex_ordinals[1];

        let ensure = facade
            .ensure_local_capacity_for_incoming_live_entries_and_write(
                src,
                src_mapping.forward_ordinal,
                dst,
                dst_mapping.reverse_ordinal,
                1,
                &[src, dst],
                &[Vec::new(), Vec::new()],
                &memory,
            )
            .expect("ensure capacity");
        let insert = facade
            .insert_edge_pair_with_local_rebalance_and_write(
                9901,
                src,
                src_mapping.forward_ordinal,
                dst,
                dst_mapping.reverse_ordinal,
                7,
                &[src, dst],
                &[vec![9901], Vec::new()],
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
    fn facade_can_scan_property_index_directly_from_stable_memory_when_clean() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let node_id = NodeId::from(51u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(888, "weight", &Value::Int64(5))
            .expect("set edge property");
        facade
            .try_write_all_to_stable_memory(&memory)
            .expect("write all");

        assert_eq!(
            facade
                .try_scan_node_ids_by_property_eq_from_stable_memory(
                    &memory,
                    "uid",
                    &Value::Text("alice".into()),
                )
                .expect("scan node equality from stable memory"),
            vec![node_id]
        );
        assert_eq!(
            facade
                .try_scan_node_ids_by_property_from_stable_memory(&memory, "uid")
                .expect("scan node property from stable memory"),
            vec![node_id]
        );
        assert_eq!(
            facade
                .try_scan_edge_ids_by_property_eq_from_stable_memory(
                    &memory,
                    "weight",
                    &Value::Int64(5),
                )
                .expect("scan edge equality from stable memory"),
            vec![888]
        );
        assert_eq!(
            facade
                .try_scan_edge_ids_by_property_from_stable_memory(&memory, "weight")
                .expect("scan edge property from stable memory"),
            vec![888]
        );
    }

    #[test]
    fn facade_prefers_hydrated_property_index_when_dirty() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let node_id = NodeId::from(61u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(999, "weight", &Value::Int64(7))
            .expect("set edge property");

        assert!(facade.node_property_store_is_dirty());
        assert!(facade.edge_property_store_is_dirty());

        assert_eq!(
            facade.scan_node_ids_by_property_eq_preferring_stable_memory(
                &memory,
                "uid",
                &Value::Text("alice".into()),
            ),
            vec![node_id]
        );
        assert_eq!(
            facade.scan_node_ids_by_property_preferring_stable_memory(&memory, "uid"),
            vec![node_id]
        );
        assert_eq!(
            facade.scan_edge_ids_by_property_eq_preferring_stable_memory(
                &memory,
                "weight",
                &Value::Int64(7),
            ),
            vec![999]
        );
        assert_eq!(
            facade.scan_edge_ids_by_property_preferring_stable_memory(&memory, "weight"),
            vec![999]
        );
    }

    #[test]
    fn facade_property_index_hydrate_prefers_paged_node_store_over_empty_snapshot() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let node_id = NodeId::from(41u8);

        facade
            .set_node_property_value(node_id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        facade
            .set_edge_property_value(777, "weight", &Value::Int64(5))
            .expect("set edge property");
        facade
            .try_write_all_to_stable_memory(&memory)
            .expect("write all");

        let image = PropertyIndexStorageImage {
            snapshot: PropertyIndexSnapshot::empty(64),
            node_store: facade.node_property_index_nodes.clone(),
            edge_store: facade.edge_property_index_nodes.clone(),
        };
        write_property_index_storage_image_to_stable_memory(&mut facade.manager, &memory, &image)
            .expect("overwrite property index image");

        let rehydrated =
            RewriteGraphPma::hydrate_from_stable_memory(facade.manager.clone(), &memory).unwrap();
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
        let mut facade = RewriteGraphPma::hydrate_from_stable_memory(manager, &memory).unwrap();
        facade
            .graph
            .insert_base_edge_pair(77, NodeId::from(1u8), 0, NodeId::from(2u8), 0, 7)
            .expect("seed sidecar");

        let mut batch = facade.begin_batch_mutation(&memory);
        let replaced = batch
            .replace_edge_pair(77, NodeId::from(1u8), 0, 0, NodeId::from(3u8), 0, 0, 9)
            .expect("replace");
        assert_eq!(replaced.0, GraphMutationPath::Base);

        let tombstoned = batch
            .tombstone_edge_pair(77, NodeId::from(1u8), 0, 0, NodeId::from(3u8), 0, 0)
            .expect("tombstone");
        assert_eq!(tombstoned, GraphMutationPath::Base);

        let refreshed = batch.flush().expect("flush");
        assert!(refreshed.0.contains(&0));
        assert!(refreshed.1.contains(&0));
    }

    #[test]
    fn facade_replace_and_tombstone_convenience_methods_write_back() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = RewriteGraphPma::hydrate_from_stable_memory(manager, &memory).unwrap();
        facade
            .graph
            .insert_base_edge_pair(77, NodeId::from(1u8), 0, NodeId::from(2u8), 0, 7)
            .expect("seed sidecar");

        let replace_summary: RewriteGraphMutationWriteSummary<_> = facade
            .replace_edge_pair_and_write(
                77,
                NodeId::from(1u8),
                0,
                0,
                NodeId::from(3u8),
                0,
                0,
                9,
                &memory,
            )
            .expect("replace and write");
        assert_eq!(replace_summary.mutation.0, GraphMutationPath::Base);

        let tombstone_summary = facade
            .tombstone_edge_pair_and_write(
                77,
                NodeId::from(1u8),
                0,
                0,
                NodeId::from(3u8),
                0,
                0,
                &memory,
            )
            .expect("tombstone and write");
        assert_eq!(tombstone_summary.mutation, GraphMutationPath::Base);
        assert!(tombstone_summary.refreshed.forward.contains(&0));
        assert!(tombstone_summary.refreshed.reverse.contains(&0));
    }

    #[test]
    fn facade_try_rebuild_locator_sidecar_rejects_mismatched_inputs() {
        let (manager, memory) = seeded_manager_and_memory();
        let mut facade = RewriteGraphPma::hydrate_from_stable_memory(manager, &memory).unwrap();

        let err = facade
            .try_rebuild_locator_sidecar(&[NodeId::from(1u8)], &[])
            .expect_err("mismatched ids should fail");
        assert_eq!(err, RewriteGraphPmaError::InvalidLocatorInputs);
    }

    #[test]
    fn facade_can_hydrate_with_locator_sidecar_in_one_step() {
        let (manager, memory) = seeded_manager_and_memory();
        let facade = RewriteGraphPma::hydrate_from_stable_memory_with_locator_sidecar(
            manager,
            &memory,
            &[NodeId::from(1u8)],
            &[vec![77]],
        )
        .expect("hydrate with sidecar");

        assert_eq!(
            facade.graph.locator(77),
            Some(crate::low_level::EdgeLocator::new(
                crate::low_level::SurfaceKind::Forward,
                NodeId::from(1u8),
                0,
            ))
        );
    }

    #[test]
    fn facade_can_bootstrap_empty_graph() {
        let memory = VecMemory::default();
        let facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap empty");

        assert!(facade.graph.forward.0.vertices.is_empty());
        assert!(facade.graph.forward.0.base_entries.is_empty());
        assert!(facade.graph.reverse.0.vertices.is_empty());
        assert!(facade.graph.reverse.0.base_entries.is_empty());
        assert!(facade
            .manager
            .layout
            .region(RegionKind::ForwardVertexTable)
            .is_some());
        assert!(facade
            .manager
            .layout
            .region(RegionKind::ReverseSegmentLog)
            .is_some());
    }

    #[test]
    fn facade_can_append_empty_vertex_pair_and_write() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");

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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");

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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");

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
            facade.graph.locator(77),
            Some(crate::low_level::EdgeLocator::new(
                crate::low_level::SurfaceKind::Forward,
                NodeId::from(1u8),
                0,
            ))
        );
    }

    #[test]
    fn facade_can_bootstrap_multiple_vertices_and_edges() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");

        let summary: RewriteBootstrapGraphWriteSummary = facade
            .bootstrap_vertices_and_edges_and_write(
                &[NodeId::from(1u8), NodeId::from(2u8), NodeId::from(3u8)],
                &[(77, 0, 1, 9), (88, 1, 2, 11)],
                &memory,
            )
            .expect("bootstrap graph");

        assert_eq!(
            summary.vertex_ordinals,
            vec![
                RewriteVertexOrdinalMapping {
                    vertex_id: NodeId::from(1u8),
                    forward_ordinal: 0,
                    reverse_ordinal: 0,
                },
                RewriteVertexOrdinalMapping {
                    vertex_id: NodeId::from(2u8),
                    forward_ordinal: 1,
                    reverse_ordinal: 1,
                },
                RewriteVertexOrdinalMapping {
                    vertex_id: NodeId::from(3u8),
                    forward_ordinal: 2,
                    reverse_ordinal: 2,
                },
            ]
        );
        assert_eq!(summary.inserts.len(), 2);
        assert_eq!(
            summary.locators,
            vec![
                RewriteEdgeLocatorMapping {
                    edge_id: 77,
                    canonical: crate::low_level::EdgeLocator::new(
                        crate::low_level::SurfaceKind::Forward,
                        NodeId::from(1u8),
                        0,
                    ),
                    forward: crate::low_level::EdgeLocator::new(
                        crate::low_level::SurfaceKind::Forward,
                        NodeId::from(1u8),
                        0,
                    ),
                    reverse: crate::low_level::EdgeLocator::new(
                        crate::low_level::SurfaceKind::Reverse,
                        NodeId::from(2u8),
                        0,
                    ),
                },
                RewriteEdgeLocatorMapping {
                    edge_id: 88,
                    canonical: crate::low_level::EdgeLocator::new(
                        crate::low_level::SurfaceKind::Forward,
                        NodeId::from(2u8),
                        0,
                    ),
                    forward: crate::low_level::EdgeLocator::new(
                        crate::low_level::SurfaceKind::Forward,
                        NodeId::from(2u8),
                        0,
                    ),
                    reverse: crate::low_level::EdgeLocator::new(
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
            facade.graph.locator(77),
            Some(crate::low_level::EdgeLocator::new(
                crate::low_level::SurfaceKind::Forward,
                NodeId::from(1u8),
                0,
            ))
        );
        assert_eq!(
            facade.graph.locator(88),
            Some(crate::low_level::EdgeLocator::new(
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
            let bootstrap = store.bootstrap_vertices_and_edges_and_write(
                &[NodeId::from(11u8), NodeId::from(12u8)],
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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let counts = touch_store(&mut facade, &memory).expect("touch via trait");
        assert_eq!(counts, (4, 4, 1, 1));
        assert_eq!(
            RewriteGraphStore::formatted_last_write_event(&facade),
            Some("bootstrap-graph vertices=2 edges=1 refreshed=(1,1) fwd=[2] rev=[3]".to_owned())
        );
    }

    #[test]
    fn rewrite_graph_store_adapter_can_bootstrap_via_trait_boundary() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut adapter: RewriteGraphStoreAdapter<'_, _, _> = facade.bind(&memory);

        let summary = adapter
            .bootstrap_vertices_and_edges(
                &[NodeId::from(21u8), NodeId::from(22u8)],
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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut adapter: RewriteGraphStoreAdapter<'_, _, _> = facade.bind(&memory);

        let bootstrap = adapter
            .bootstrap_vertices_and_edges(
                &[NodeId::from(31u8), NodeId::from(32u8)],
                &[(902, 0, 1, 5)],
            )
            .expect("bootstrap through adapter");

        let src = bootstrap.vertex_ordinals[0];
        let dst = bootstrap.vertex_ordinals[1];

        let replaced = adapter
            .replace_edge_pair(
                902,
                src.vertex_id,
                src.forward_ordinal,
                0,
                NodeId::from(33u8),
                dst.reverse_ordinal,
                0,
                7,
            )
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
            .tombstone_edge_pair(
                902,
                src.vertex_id,
                src.forward_ordinal,
                0,
                NodeId::from(33u8),
                dst.reverse_ordinal,
                0,
            )
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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut adapter = facade.bind(&memory);

        let mut batch = adapter.begin_batch_mutation();
        let refreshed = batch.flush().expect("flush empty batch");
        assert_eq!(refreshed, (Vec::new(), Vec::new()));
    }

    #[test]
    fn rewrite_graph_service_trait_can_drive_bootstrap_and_flush() {
        fn use_service(
            service: &mut impl RewriteGraphService,
        ) -> RewriteGraphPmaResult<(usize, usize, bool, RewriteBootstrapGraphProjection)> {
            let summary = service.bootstrap_vertices_and_edges(
                &[NodeId::from(41u8), NodeId::from(42u8)],
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
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut adapter = facade.bind(&memory);
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
