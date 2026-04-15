use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gleaph_graph_kernel::EdgeId;

use crate::low_level::{
    EdgeInsertPath, GraphEnsureCapacitySegmentWriteSummary, GraphEnsureCapacityWriteSummary,
    GraphInsertResult, GraphInsertSegmentWriteSummary, GraphInsertWriteSummary,
    GraphMaintenanceBatchWriteSummary, GraphMaintenanceCycleWriteSummary,
    GraphMaintenanceQueueStorageSnapshot, GraphMaintenanceWorkItem, GraphMutationPath,
    LogicalEdgeLocator, VertexRef,
};
use crate::property_index::{
    PropertyIndexNodeId, PropertyIndexNodeStoreDelta, PropertyIndexNodeStoreMutationKind,
};

type GraphStoreReplaceEdgeSummary =
    super::GraphStoreMutationWriteSummary<(GraphMutationPath, (super::EdgeEntry, super::EdgeEntry))>;

/// Structured reason for why property-index mutation fell back to full node-store rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PropertyIndexFallbackReason {
    NodeUpsertLocalUnavailable,
    NodeRemoveLocalUnavailable,
    EdgeUpsertLocalUnavailable,
    EdgeRemoveLocalUnavailable,
}

/// Snapshot of graph persistence production metrics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphStoreProductionMetricsSnapshot {
    pub property_index_fallback_total: u64,
    pub property_index_fallback_by_reason: BTreeMap<PropertyIndexFallbackReason, u64>,
    pub maintenance_queue_rebuild_total: u64,
    pub maintenance_queue_refresh_total: u64,
    pub maintenance_queued_batch_total: u64,
    pub maintenance_queue_write_total: u64,
    pub maintenance_queue_last_persisted_bytes: u64,
    pub maintenance_queue_format_version: u32,
    pub node_eq_scan_count: u64,
    pub edge_eq_scan_count: u64,
    pub node_eq_scan_total_nanos: u128,
    pub edge_eq_scan_total_nanos: u128,
    pub node_eq_scan_p50_nanos: u64,
    pub node_eq_scan_p95_nanos: u64,
    pub edge_eq_scan_p50_nanos: u64,
    pub edge_eq_scan_p95_nanos: u64,
}

#[derive(Clone, Debug, Default)]
struct GraphStoreProductionMetricsInner {
    property_index_fallback_total: u64,
    property_index_fallback_by_reason: BTreeMap<PropertyIndexFallbackReason, u64>,
    maintenance_queue_rebuild_total: u64,
    maintenance_queue_refresh_total: u64,
    maintenance_queued_batch_total: u64,
    maintenance_queue_write_total: u64,
    maintenance_queue_last_persisted_bytes: u64,
    maintenance_queue_format_version: u32,
    node_eq_scan_nanos: Vec<u64>,
    edge_eq_scan_nanos: Vec<u64>,
}

/// Mutable in-process metrics store with cheap clone semantics.
#[derive(Clone, Debug, Default)]
pub struct GraphStoreProductionMetrics {
    inner: Arc<Mutex<GraphStoreProductionMetricsInner>>,
}

/// Result of one convenience mutation that also flushed dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreMutationWriteSummary<T> {
    pub mutation: T,
    pub refreshed: GraphStoreRefreshedVertices,
}

/// Vertices whose label sidecars were refreshed during one facade-level writeback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreRefreshedVertices {
    pub forward: Vec<usize>,
    pub reverse: Vec<usize>,
}

/// Result of draining stable dirty ordinal intervals into the in-memory maintenance queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreMaintenanceDirtyDrainSummary {
    pub intervals_drained: usize,
    pub work_items_merged: usize,
    pub queue_len_after: usize,
    pub budget_exhausted: bool,
    /// Instructions consumed during this drain when a budget was supplied; otherwise zero.
    pub instructions_used: u64,
}

/// Volatile property-store / PIDX backlog for timer-driven maintenance (separate from ordinal dirty).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphStorePropertyMaintenanceBacklog {
    pub property_index_dirty: bool,
    pub node_property_store_dirty: bool,
    pub edge_property_store_dirty: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreAppendVertexWriteSummary {
    pub ordinals: (usize, usize),
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreAppendVerticesWriteSummary {
    pub ordinals: Vec<(usize, usize)>,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphStoreVertexOrdinalMapping {
    pub vertex_ref: VertexRef,
    pub forward_ordinal: usize,
    pub reverse_ordinal: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreBootstrapEdgeWriteSummary {
    pub ordinals: (usize, usize),
    pub insert: GraphInsertResult,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphStoreEdgeLogicalLocatorMapping {
    pub edge_id: EdgeId,
    pub canonical: LogicalEdgeLocator,
    pub forward: LogicalEdgeLocator,
    pub reverse: LogicalEdgeLocator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreBootstrapGraphWriteSummary {
    pub vertex_ordinals: Vec<GraphStoreVertexOrdinalMapping>,
    pub inserts: Vec<GraphInsertResult>,
    pub locators: Vec<GraphStoreEdgeLogicalLocatorMapping>,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreBootstrapGraphProjection {
    pub vertex_ordinals: Vec<GraphStoreVertexOrdinalMapping>,
    pub locators: Vec<GraphStoreEdgeLogicalLocatorMapping>,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreBootstrapVerticesProjection {
    pub ordinals: Vec<(usize, usize)>,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreBootstrapEdgeProjection {
    pub path: Option<EdgeInsertPath>,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphStoreEdgeWriteOperation {
    ReplaceLabel,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreEdgeWriteProjection {
    pub operation: GraphStoreEdgeWriteOperation,
    pub path: GraphMutationPath,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreNodeDeleteProjection {
    pub detached: bool,
    pub deleted_edge_ids: Vec<EdgeId>,
    pub edge_writes: Vec<GraphStoreEdgeWriteProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreEnsureCapacityProjection {
    pub rebalanced: bool,
    pub total_displacement: i64,
    pub max_displacement: i64,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreInsertEdgeProjection {
    pub inserted: bool,
    pub path: Option<EdgeInsertPath>,
    pub rebalanced: bool,
    pub total_displacement: i64,
    pub max_displacement: i64,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreMaintenanceCycleProjection {
    pub vertex_ref: VertexRef,
    pub ordinal: usize,
    pub window_start_ordinal: usize,
    pub window_end_ordinal_exclusive: usize,
    pub priority_score: u64,
    pub last_maintenance_epoch: Option<u64>,
    pub recent_maintenance_penalty: u64,
    pub direct_overflow_total: usize,
    pub window_overflow_total: usize,
    pub reclaimable_tombstones_total: usize,
    pub window_total_base_slots: usize,
    pub total_displacement: i64,
    pub max_displacement: i64,
    pub refreshed: GraphStoreRefreshedVertices,
    pub queue_storage_before: Option<GraphStoreMaintenanceQueueStorageProjection>,
    pub queue_storage_after: Option<GraphStoreMaintenanceQueueStorageProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreMaintenanceBatchProjection {
    pub cycles: usize,
    pub queue_len_before: usize,
    pub queue_len_after: usize,
    pub swept_forward_segments: usize,
    pub swept_reverse_segments: usize,
    pub queue_storage_before: Option<GraphStoreMaintenanceQueueStorageProjection>,
    pub queue_storage_after: Option<GraphStoreMaintenanceQueueStorageProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreMaintenanceQueueItemProjection {
    pub vertex_ref: VertexRef,
    pub anchor_ordinal: usize,
    pub window_start_ordinal: usize,
    pub window_end_ordinal_exclusive: usize,
    pub priority_score: u64,
    pub last_maintenance_epoch: Option<u64>,
    pub recent_maintenance_penalty: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphStoreMaintenanceQueueAction {
    Rebuild,
    Refresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreMaintenanceQueueProjection {
    pub action: GraphStoreMaintenanceQueueAction,
    pub queue_len_before: usize,
    pub queue_len_after: usize,
    pub persisted_bytes: u64,
    pub format_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreMaintenanceQueueStorageProjection {
    pub logical_len_bytes: u64,
    pub queue_len: usize,
    pub format_version: Option<u32>,
    pub stored_checksum: Option<u64>,
    pub computed_checksum: Option<u64>,
    pub checksum_valid: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStorePropertyWriteProjection {
    pub sections: GraphStorePropertyIndexTouchedSections,
    pub node_store_operations: Vec<PropertyIndexNodeStoreMutationKind>,
    pub fallback_reasons: Vec<PropertyIndexFallbackReason>,
    pub touched_node_ids: Vec<PropertyIndexNodeId>,
    pub allocated_node_ids: Vec<PropertyIndexNodeId>,
    pub freed_node_ids: Vec<PropertyIndexNodeId>,
    pub flushed_sections: GraphStorePropertyIndexTouchedSections,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphStoreWriteEventProjection {
    BootstrapVertices(GraphStoreBootstrapVerticesProjection),
    BootstrapEdge(GraphStoreBootstrapEdgeProjection),
    BootstrapGraph(GraphStoreBootstrapGraphProjection),
    EnsureCapacity(GraphStoreEnsureCapacityProjection),
    InsertEdge(GraphStoreInsertEdgeProjection),
    MaintenanceCycle(GraphStoreMaintenanceCycleProjection),
    MaintenanceBatch(GraphStoreMaintenanceBatchProjection),
    MaintenanceQueue(GraphStoreMaintenanceQueueProjection),
    Property(GraphStorePropertyWriteProjection),
    Edge(GraphStoreEdgeWriteProjection),
    NodeDelete(GraphStoreNodeDeleteProjection),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphStorePropertyIndexTouchedSections {
    pub property_store: bool,
    pub logical_index: bool,
    pub node_store: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStorePropertyIndexMutationSummary {
    pub sections: GraphStorePropertyIndexTouchedSections,
    pub node_store_operations: Vec<PropertyIndexNodeStoreMutationKind>,
    pub fallback_reasons: Vec<PropertyIndexFallbackReason>,
    pub touched_node_ids: Vec<PropertyIndexNodeId>,
    pub allocated_node_ids: Vec<PropertyIndexNodeId>,
    pub freed_node_ids: Vec<PropertyIndexNodeId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStorePropertyMutationWriteSummary {
    pub mutation: GraphStorePropertyIndexMutationSummary,
    pub flushed_sections: GraphStorePropertyIndexTouchedSections,
    pub refreshed: GraphStoreRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphStoreFacadeWriteEvent {
    AppendVertex(GraphStoreAppendVertexWriteSummary),
    AppendVertices(GraphStoreAppendVerticesWriteSummary),
    BootstrapEdge(GraphStoreBootstrapEdgeWriteSummary),
    BootstrapGraph(GraphStoreBootstrapGraphWriteSummary),
    Property(GraphStorePropertyMutationWriteSummary),
    EnsureCapacity(GraphEnsureCapacityWriteSummary),
    EnsureCapacitySegment(GraphEnsureCapacitySegmentWriteSummary),
    InsertEdge(GraphInsertWriteSummary),
    InsertEdgeSegment(GraphInsertSegmentWriteSummary),
    MaintenanceCycle(GraphMaintenanceCycleWriteSummary),
    MaintenanceBatch(GraphMaintenanceBatchWriteSummary),
    MaintenanceQueue(GraphStoreMaintenanceQueueProjection),
    ReplaceEdge(GraphStoreReplaceEdgeSummary),
    DeleteEdge(GraphStoreMutationWriteSummary<GraphMutationPath>),
}

impl GraphStoreBootstrapGraphWriteSummary {
    pub fn projection(&self) -> GraphStoreBootstrapGraphProjection {
        GraphStoreBootstrapGraphProjection {
            vertex_ordinals: self.vertex_ordinals.clone(),
            locators: self.locators.clone(),
            refreshed: self.refreshed.clone(),
        }
    }
}

impl GraphStoreBootstrapVerticesProjection {
    pub(crate) fn from_single_summary(summary: &GraphStoreAppendVertexWriteSummary) -> Self {
        Self {
            ordinals: vec![summary.ordinals],
            refreshed: summary.refreshed.clone(),
        }
    }

    fn from_many_summary(summary: &GraphStoreAppendVerticesWriteSummary) -> Self {
        Self {
            ordinals: summary.ordinals.clone(),
            refreshed: summary.refreshed.clone(),
        }
    }
}

impl GraphStoreBootstrapEdgeProjection {
    pub(crate) fn from_facade_summary(summary: &GraphStoreBootstrapEdgeWriteSummary) -> Self {
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

impl GraphStoreFacadeWriteEvent {
    pub fn shared_projections(&self) -> Vec<GraphStoreWriteEventProjection> {
        self.shared_projection().into_iter().collect()
    }

    pub fn shared_projection(&self) -> Option<GraphStoreWriteEventProjection> {
        match self {
            Self::AppendVertex(summary) => Some(GraphStoreWriteEventProjection::BootstrapVertices(
                GraphStoreBootstrapVerticesProjection::from_single_summary(summary),
            )),
            Self::AppendVertices(summary) => Some(GraphStoreWriteEventProjection::BootstrapVertices(
                GraphStoreBootstrapVerticesProjection::from_many_summary(summary),
            )),
            Self::BootstrapEdge(summary) => Some(GraphStoreWriteEventProjection::BootstrapEdge(
                GraphStoreBootstrapEdgeProjection::from_facade_summary(summary),
            )),
            Self::BootstrapGraph(summary) => Some(GraphStoreWriteEventProjection::BootstrapGraph(
                summary.projection(),
            )),
            Self::EnsureCapacity(summary) => Some(GraphStoreWriteEventProjection::EnsureCapacity(
                GraphStoreEnsureCapacityProjection::from_summary(summary),
            )),
            Self::EnsureCapacitySegment(summary) => {
                Some(GraphStoreWriteEventProjection::EnsureCapacity(
                    GraphStoreEnsureCapacityProjection::from_segment_summary(summary),
                ))
            }
            Self::InsertEdge(summary) => Some(GraphStoreWriteEventProjection::InsertEdge(
                GraphStoreInsertEdgeProjection::from_summary(summary),
            )),
            Self::InsertEdgeSegment(summary) => Some(GraphStoreWriteEventProjection::InsertEdge(
                GraphStoreInsertEdgeProjection::from_segment_summary(summary),
            )),
            Self::MaintenanceCycle(summary) => {
                Some(GraphStoreWriteEventProjection::MaintenanceCycle(
                    GraphStoreMaintenanceCycleProjection::from_summary(summary),
                ))
            }
            Self::MaintenanceBatch(summary) => {
                Some(GraphStoreWriteEventProjection::MaintenanceBatch(
                    GraphStoreMaintenanceBatchProjection::from_summary(summary),
                ))
            }
            Self::MaintenanceQueue(summary) => Some(
                GraphStoreWriteEventProjection::MaintenanceQueue(summary.clone()),
            ),
            Self::Property(summary) => {
                Some(GraphStoreWriteEventProjection::Property(summary.projection()))
            }
            Self::ReplaceEdge(_) | Self::DeleteEdge(_) => self
                .edge_projection()
                .map(GraphStoreWriteEventProjection::Edge),
        }
    }

    pub fn edge_projection(&self) -> Option<GraphStoreEdgeWriteProjection> {
        match self {
            Self::ReplaceEdge(summary) => Some(GraphStoreEdgeWriteProjection {
                operation: GraphStoreEdgeWriteOperation::ReplaceLabel,
                path: summary.mutation.0,
                refreshed: summary.refreshed.clone(),
            }),
            Self::DeleteEdge(summary) => Some(GraphStoreEdgeWriteProjection {
                operation: GraphStoreEdgeWriteOperation::Delete,
                path: summary.mutation,
                refreshed: summary.refreshed.clone(),
            }),
            _ => None,
        }
    }

    pub fn property_projection(&self) -> Option<GraphStorePropertyWriteProjection> {
        match self {
            Self::Property(summary) => Some(summary.projection()),
            _ => None,
        }
    }
}

const METRICS_SAMPLE_LIMIT: usize = 2048;

fn percentile_nanos(values: &[u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

impl GraphStoreProductionMetrics {
    fn push_capped(samples: &mut Vec<u64>, value: u64) {
        if samples.len() >= METRICS_SAMPLE_LIMIT {
            samples.remove(0);
        }
        samples.push(value);
    }

    pub fn record_property_index_fallback(&self, reason: PropertyIndexFallbackReason) {
        let mut inner = self.inner.lock().expect("metrics lock poisoned");
        inner.property_index_fallback_total += 1;
        *inner
            .property_index_fallback_by_reason
            .entry(reason)
            .or_insert(0) += 1;
    }

    pub fn record_node_eq_scan_nanos(&self, nanos: u64) {
        let mut inner = self.inner.lock().expect("metrics lock poisoned");
        Self::push_capped(&mut inner.node_eq_scan_nanos, nanos);
    }

    pub fn record_edge_eq_scan_nanos(&self, nanos: u64) {
        let mut inner = self.inner.lock().expect("metrics lock poisoned");
        Self::push_capped(&mut inner.edge_eq_scan_nanos, nanos);
    }

    pub fn record_maintenance_queue_rebuild(&self) {
        let mut inner = self.inner.lock().expect("metrics lock poisoned");
        inner.maintenance_queue_rebuild_total += 1;
    }

    pub fn record_maintenance_queue_refresh(&self) {
        let mut inner = self.inner.lock().expect("metrics lock poisoned");
        inner.maintenance_queue_refresh_total += 1;
    }

    pub fn record_maintenance_queued_batch(&self) {
        let mut inner = self.inner.lock().expect("metrics lock poisoned");
        inner.maintenance_queued_batch_total += 1;
    }

    pub fn record_maintenance_queue_write(&self, persisted_bytes: u64, format_version: u32) {
        let mut inner = self.inner.lock().expect("metrics lock poisoned");
        inner.maintenance_queue_write_total += 1;
        inner.maintenance_queue_last_persisted_bytes = persisted_bytes;
        inner.maintenance_queue_format_version = format_version;
    }

    pub fn snapshot(&self) -> GraphStoreProductionMetricsSnapshot {
        let inner = self.inner.lock().expect("metrics lock poisoned");
        let node_total = inner
            .node_eq_scan_nanos
            .iter()
            .fold(0u128, |acc, n| acc + u128::from(*n));
        let edge_total = inner
            .edge_eq_scan_nanos
            .iter()
            .fold(0u128, |acc, n| acc + u128::from(*n));
        GraphStoreProductionMetricsSnapshot {
            property_index_fallback_total: inner.property_index_fallback_total,
            property_index_fallback_by_reason: inner.property_index_fallback_by_reason.clone(),
            maintenance_queue_rebuild_total: inner.maintenance_queue_rebuild_total,
            maintenance_queue_refresh_total: inner.maintenance_queue_refresh_total,
            maintenance_queued_batch_total: inner.maintenance_queued_batch_total,
            maintenance_queue_write_total: inner.maintenance_queue_write_total,
            maintenance_queue_last_persisted_bytes: inner.maintenance_queue_last_persisted_bytes,
            maintenance_queue_format_version: inner.maintenance_queue_format_version,
            node_eq_scan_count: inner.node_eq_scan_nanos.len() as u64,
            edge_eq_scan_count: inner.edge_eq_scan_nanos.len() as u64,
            node_eq_scan_total_nanos: node_total,
            edge_eq_scan_total_nanos: edge_total,
            node_eq_scan_p50_nanos: percentile_nanos(&inner.node_eq_scan_nanos, 0.50),
            node_eq_scan_p95_nanos: percentile_nanos(&inner.node_eq_scan_nanos, 0.95),
            edge_eq_scan_p50_nanos: percentile_nanos(&inner.edge_eq_scan_nanos, 0.50),
            edge_eq_scan_p95_nanos: percentile_nanos(&inner.edge_eq_scan_nanos, 0.95),
        }
    }
}

impl GraphStoreRefreshedVertices {
    pub fn new(forward: Vec<usize>, reverse: Vec<usize>) -> Self {
        Self { forward, reverse }
    }

    pub fn from_slices(forward: &[usize], reverse: &[usize]) -> Self {
        Self {
            forward: forward.to_vec(),
            reverse: reverse.to_vec(),
        }
    }
}

impl GraphStorePropertyIndexMutationSummary {
    pub(crate) fn from_delta(
        delta: PropertyIndexNodeStoreDelta,
        node_store_operations: Vec<PropertyIndexNodeStoreMutationKind>,
        fallback_reasons: Vec<PropertyIndexFallbackReason>,
    ) -> Self {
        let node_store = !delta.touched_node_ids.is_empty();
        Self {
            sections: GraphStorePropertyIndexTouchedSections {
                property_store: true,
                logical_index: true,
                node_store,
            },
            node_store_operations,
            fallback_reasons,
            touched_node_ids: delta.touched_node_ids,
            allocated_node_ids: delta.allocated_node_ids,
            freed_node_ids: delta.freed_node_ids,
        }
    }
}

impl GraphStorePropertyMutationWriteSummary {
    /// Property mutation recorded before stable writeback (until `flush` on the graph).
    pub fn pending_from_mutation(mutation: GraphStorePropertyIndexMutationSummary) -> Self {
        Self {
            mutation,
            flushed_sections: GraphStorePropertyIndexTouchedSections {
                property_store: false,
                logical_index: false,
                node_store: false,
            },
            refreshed: GraphStoreRefreshedVertices::new(Vec::new(), Vec::new()),
        }
    }

    pub fn is_pending_stable_flush(&self) -> bool {
        !self.flushed_sections.property_store
            && !self.flushed_sections.logical_index
            && !self.flushed_sections.node_store
    }

    pub fn projection(&self) -> GraphStorePropertyWriteProjection {
        GraphStorePropertyWriteProjection {
            sections: self.mutation.sections,
            node_store_operations: self.mutation.node_store_operations.clone(),
            fallback_reasons: self.mutation.fallback_reasons.clone(),
            touched_node_ids: self.mutation.touched_node_ids.clone(),
            allocated_node_ids: self.mutation.allocated_node_ids.clone(),
            freed_node_ids: self.mutation.freed_node_ids.clone(),
            flushed_sections: self.flushed_sections,
            refreshed: self.refreshed.clone(),
        }
    }

    pub(crate) fn from_mutation_and_refresh(
        mutation: GraphStorePropertyIndexMutationSummary,
        refreshed_forward_vertices: Vec<usize>,
        refreshed_reverse_vertices: Vec<usize>,
    ) -> Self {
        Self {
            flushed_sections: mutation.sections,
            mutation,
            refreshed: GraphStoreRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        }
    }
}

impl GraphStoreEnsureCapacityProjection {
    pub(crate) fn from_summary(summary: &GraphEnsureCapacityWriteSummary) -> Self {
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
            refreshed: GraphStoreRefreshedVertices::new(
                summary.refreshed_forward_vertices.clone(),
                summary.refreshed_reverse_vertices.clone(),
            ),
        }
    }

    pub(crate) fn from_segment_summary(summary: &GraphEnsureCapacitySegmentWriteSummary) -> Self {
        let (total_displacement, max_displacement) = summary
            .rebalance
            .as_ref()
            .map(|rebalance| {
                (
                    rebalance.apply.apply.total_displacement(),
                    rebalance.apply.apply.max_displacement(),
                )
            })
            .unwrap_or((0, 0));
        Self {
            rebalanced: summary.rebalanced,
            total_displacement,
            max_displacement,
            refreshed: GraphStoreRefreshedVertices::new(
                summary.refreshed_forward_vertices.clone(),
                summary.refreshed_reverse_vertices.clone(),
            ),
        }
    }
}

impl GraphStoreInsertEdgeProjection {
    pub(crate) fn from_summary(summary: &GraphInsertWriteSummary) -> Self {
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
            refreshed: GraphStoreRefreshedVertices::new(
                summary.refreshed_forward_vertices.clone(),
                summary.refreshed_reverse_vertices.clone(),
            ),
        }
    }

    pub(crate) fn from_segment_summary(summary: &GraphInsertSegmentWriteSummary) -> Self {
        let path = summary
            .insert
            .as_ref()
            .and_then(|insert| match insert {
                GraphInsertResult::Inserted { path, .. } => Some(path),
                GraphInsertResult::RebalanceRequired(_) => None,
            })
            .copied();
        let (total_displacement, max_displacement) = summary
            .rebalance
            .as_ref()
            .map(|rebalance| {
                (
                    rebalance.apply.apply.total_displacement(),
                    rebalance.apply.apply.max_displacement(),
                )
            })
            .unwrap_or((0, 0));
        Self {
            inserted: summary.insert.is_some(),
            path,
            rebalanced: summary.rebalance.is_some(),
            total_displacement,
            max_displacement,
            refreshed: GraphStoreRefreshedVertices::new(
                summary.refreshed_forward_vertices.clone(),
                summary.refreshed_reverse_vertices.clone(),
            ),
        }
    }
}

impl GraphStoreMaintenanceCycleProjection {
    fn storage_from_snapshot(
        snapshot: Option<GraphMaintenanceQueueStorageSnapshot>,
    ) -> Option<GraphStoreMaintenanceQueueStorageProjection> {
        snapshot.map(|snapshot| GraphStoreMaintenanceQueueStorageProjection {
            logical_len_bytes: snapshot.logical_len_bytes,
            queue_len: snapshot.queue_len,
            format_version: snapshot.format_version,
            stored_checksum: None,
            computed_checksum: None,
            checksum_valid: snapshot.checksum_valid,
        })
    }

    pub(crate) fn from_summary(summary: &GraphMaintenanceCycleWriteSummary) -> Self {
        let candidate = summary.candidate;
        Self {
            vertex_ref: candidate.vertex_ref,
            ordinal: candidate.ordinal,
            window_start_ordinal: summary.window_start_ordinal,
            window_end_ordinal_exclusive: summary.window_end_ordinal_exclusive,
            priority_score: candidate.priority_score,
            last_maintenance_epoch: candidate.last_maintenance_epoch,
            recent_maintenance_penalty: candidate.recent_maintenance_penalty,
            direct_overflow_total: candidate
                .forward_overflow_len
                .saturating_add(candidate.reverse_overflow_len),
            window_overflow_total: candidate
                .forward_window_overflow_entries
                .saturating_add(candidate.reverse_window_overflow_entries),
            reclaimable_tombstones_total: candidate
                .forward_reclaimable_tombstones
                .saturating_add(candidate.reverse_reclaimable_tombstones),
            window_total_base_slots: candidate
                .forward_window_total_base_slots
                .saturating_add(candidate.reverse_window_total_base_slots),
            total_displacement: summary.rebalance.apply.apply.total_displacement(),
            max_displacement: summary.rebalance.apply.apply.max_displacement(),
            refreshed: GraphStoreRefreshedVertices::new(
                summary.rebalance.refreshed_forward_vertices.clone(),
                summary.rebalance.refreshed_reverse_vertices.clone(),
            ),
            queue_storage_before: Self::storage_from_snapshot(summary.queue_storage_before),
            queue_storage_after: Self::storage_from_snapshot(summary.queue_storage_after),
        }
    }
}

impl GraphStoreMaintenanceBatchProjection {
    pub(crate) fn from_summary(summary: &GraphMaintenanceBatchWriteSummary) -> Self {
        Self {
            cycles: summary.cycles.len(),
            queue_len_before: summary.queue_len_before,
            queue_len_after: summary.queue_len_after,
            swept_forward_segments: summary.swept_forward_segments.len(),
            swept_reverse_segments: summary.swept_reverse_segments.len(),
            queue_storage_before: GraphStoreMaintenanceCycleProjection::storage_from_snapshot(
                summary.queue_storage_before,
            ),
            queue_storage_after: GraphStoreMaintenanceCycleProjection::storage_from_snapshot(
                summary.queue_storage_after,
            ),
        }
    }
}

impl GraphStoreMaintenanceQueueItemProjection {
    pub(crate) fn from_work_item(work_item: GraphMaintenanceWorkItem) -> Self {
        Self {
            vertex_ref: work_item.vertex_ref,
            anchor_ordinal: work_item.anchor_ordinal,
            window_start_ordinal: work_item.start_ordinal,
            window_end_ordinal_exclusive: work_item.end_ordinal_exclusive,
            priority_score: work_item.priority_score,
            last_maintenance_epoch: work_item.last_maintenance_epoch,
            recent_maintenance_penalty: work_item.recent_maintenance_penalty,
        }
    }
}
