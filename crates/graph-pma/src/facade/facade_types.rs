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

type GraphPmaReplaceEdgeSummary = super::GraphPmaMutationWriteSummary<(
    GraphMutationPath,
    (super::EdgeEntry, super::EdgeEntry),
)>;

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
pub struct GraphPmaProductionMetricsSnapshot {
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
struct GraphPmaProductionMetricsInner {
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
pub struct GraphPmaProductionMetrics {
    inner: Arc<Mutex<GraphPmaProductionMetricsInner>>,
}

/// Result of one convenience mutation that also flushed dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaMutationWriteSummary<T> {
    pub mutation: T,
    pub refreshed: GraphPmaRefreshedVertices,
}

/// Vertices whose label sidecars were refreshed during one facade-level writeback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaRefreshedVertices {
    pub forward: Vec<usize>,
    pub reverse: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaAppendVertexWriteSummary {
    pub ordinals: (usize, usize),
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaAppendVerticesWriteSummary {
    pub ordinals: Vec<(usize, usize)>,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphPmaVertexOrdinalMapping {
    pub vertex_ref: VertexRef,
    pub forward_ordinal: usize,
    pub reverse_ordinal: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaBootstrapEdgeWriteSummary {
    pub ordinals: (usize, usize),
    pub insert: GraphInsertResult,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphPmaEdgeLogicalLocatorMapping {
    pub edge_id: EdgeId,
    pub canonical: LogicalEdgeLocator,
    pub forward: LogicalEdgeLocator,
    pub reverse: LogicalEdgeLocator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaBootstrapGraphWriteSummary {
    pub vertex_ordinals: Vec<GraphPmaVertexOrdinalMapping>,
    pub inserts: Vec<GraphInsertResult>,
    pub locators: Vec<GraphPmaEdgeLogicalLocatorMapping>,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaBootstrapGraphProjection {
    pub vertex_ordinals: Vec<GraphPmaVertexOrdinalMapping>,
    pub locators: Vec<GraphPmaEdgeLogicalLocatorMapping>,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaBootstrapVerticesProjection {
    pub ordinals: Vec<(usize, usize)>,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaBootstrapEdgeProjection {
    pub path: Option<EdgeInsertPath>,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphPmaEdgeWriteOperation {
    ReplaceLabel,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaEdgeWriteProjection {
    pub operation: GraphPmaEdgeWriteOperation,
    pub path: GraphMutationPath,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaNodeDeleteProjection {
    pub detached: bool,
    pub deleted_edge_ids: Vec<EdgeId>,
    pub edge_writes: Vec<GraphPmaEdgeWriteProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaEnsureCapacityProjection {
    pub rebalanced: bool,
    pub total_displacement: i64,
    pub max_displacement: i64,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaInsertEdgeProjection {
    pub inserted: bool,
    pub path: Option<EdgeInsertPath>,
    pub rebalanced: bool,
    pub total_displacement: i64,
    pub max_displacement: i64,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaMaintenanceCycleProjection {
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
    pub refreshed: GraphPmaRefreshedVertices,
    pub queue_storage_before: Option<GraphPmaMaintenanceQueueStorageProjection>,
    pub queue_storage_after: Option<GraphPmaMaintenanceQueueStorageProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaMaintenanceBatchProjection {
    pub cycles: usize,
    pub queue_len_before: usize,
    pub queue_len_after: usize,
    pub swept_forward_segments: usize,
    pub swept_reverse_segments: usize,
    pub queue_storage_before: Option<GraphPmaMaintenanceQueueStorageProjection>,
    pub queue_storage_after: Option<GraphPmaMaintenanceQueueStorageProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaMaintenanceQueueItemProjection {
    pub vertex_ref: VertexRef,
    pub anchor_ordinal: usize,
    pub window_start_ordinal: usize,
    pub window_end_ordinal_exclusive: usize,
    pub priority_score: u64,
    pub last_maintenance_epoch: Option<u64>,
    pub recent_maintenance_penalty: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphPmaMaintenanceQueueAction {
    Rebuild,
    Refresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaMaintenanceQueueProjection {
    pub action: GraphPmaMaintenanceQueueAction,
    pub queue_len_before: usize,
    pub queue_len_after: usize,
    pub persisted_bytes: u64,
    pub format_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaMaintenanceQueueStorageProjection {
    pub logical_len_bytes: u64,
    pub queue_len: usize,
    pub format_version: Option<u32>,
    pub stored_checksum: Option<u64>,
    pub computed_checksum: Option<u64>,
    pub checksum_valid: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaPropertyWriteProjection {
    pub sections: GraphPmaPropertyIndexTouchedSections,
    pub node_store_operations: Vec<PropertyIndexNodeStoreMutationKind>,
    pub fallback_reasons: Vec<PropertyIndexFallbackReason>,
    pub touched_node_ids: Vec<PropertyIndexNodeId>,
    pub allocated_node_ids: Vec<PropertyIndexNodeId>,
    pub freed_node_ids: Vec<PropertyIndexNodeId>,
    pub flushed_sections: GraphPmaPropertyIndexTouchedSections,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphPmaWriteEventProjection {
    BootstrapVertices(GraphPmaBootstrapVerticesProjection),
    BootstrapEdge(GraphPmaBootstrapEdgeProjection),
    BootstrapGraph(GraphPmaBootstrapGraphProjection),
    EnsureCapacity(GraphPmaEnsureCapacityProjection),
    InsertEdge(GraphPmaInsertEdgeProjection),
    MaintenanceCycle(GraphPmaMaintenanceCycleProjection),
    MaintenanceBatch(GraphPmaMaintenanceBatchProjection),
    MaintenanceQueue(GraphPmaMaintenanceQueueProjection),
    Property(GraphPmaPropertyWriteProjection),
    Edge(GraphPmaEdgeWriteProjection),
    NodeDelete(GraphPmaNodeDeleteProjection),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphPmaPropertyIndexTouchedSections {
    pub property_store: bool,
    pub logical_index: bool,
    pub node_store: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaPropertyIndexMutationSummary {
    pub sections: GraphPmaPropertyIndexTouchedSections,
    pub node_store_operations: Vec<PropertyIndexNodeStoreMutationKind>,
    pub fallback_reasons: Vec<PropertyIndexFallbackReason>,
    pub touched_node_ids: Vec<PropertyIndexNodeId>,
    pub allocated_node_ids: Vec<PropertyIndexNodeId>,
    pub freed_node_ids: Vec<PropertyIndexNodeId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaPropertyMutationWriteSummary {
    pub mutation: GraphPmaPropertyIndexMutationSummary,
    pub flushed_sections: GraphPmaPropertyIndexTouchedSections,
    pub refreshed: GraphPmaRefreshedVertices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphPmaFacadeWriteEvent {
    AppendVertex(GraphPmaAppendVertexWriteSummary),
    AppendVertices(GraphPmaAppendVerticesWriteSummary),
    BootstrapEdge(GraphPmaBootstrapEdgeWriteSummary),
    BootstrapGraph(GraphPmaBootstrapGraphWriteSummary),
    Property(GraphPmaPropertyMutationWriteSummary),
    EnsureCapacity(GraphEnsureCapacityWriteSummary),
    EnsureCapacitySegment(GraphEnsureCapacitySegmentWriteSummary),
    InsertEdge(GraphInsertWriteSummary),
    InsertEdgeSegment(GraphInsertSegmentWriteSummary),
    MaintenanceCycle(GraphMaintenanceCycleWriteSummary),
    MaintenanceBatch(GraphMaintenanceBatchWriteSummary),
    MaintenanceQueue(GraphPmaMaintenanceQueueProjection),
    ReplaceEdge(GraphPmaReplaceEdgeSummary),
    DeleteEdge(GraphPmaMutationWriteSummary<GraphMutationPath>),
}

impl GraphPmaBootstrapGraphWriteSummary {
    pub fn projection(&self) -> GraphPmaBootstrapGraphProjection {
        GraphPmaBootstrapGraphProjection {
            vertex_ordinals: self.vertex_ordinals.clone(),
            locators: self.locators.clone(),
            refreshed: self.refreshed.clone(),
        }
    }
}

impl GraphPmaBootstrapVerticesProjection {
    pub(crate) fn from_single_summary(summary: &GraphPmaAppendVertexWriteSummary) -> Self {
        Self {
            ordinals: vec![summary.ordinals],
            refreshed: summary.refreshed.clone(),
        }
    }

    fn from_many_summary(summary: &GraphPmaAppendVerticesWriteSummary) -> Self {
        Self {
            ordinals: summary.ordinals.clone(),
            refreshed: summary.refreshed.clone(),
        }
    }
}

impl GraphPmaBootstrapEdgeProjection {
    pub(crate) fn from_facade_summary(summary: &GraphPmaBootstrapEdgeWriteSummary) -> Self {
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

impl GraphPmaFacadeWriteEvent {
    pub fn shared_projections(&self) -> Vec<GraphPmaWriteEventProjection> {
        self.shared_projection().into_iter().collect()
    }

    pub fn shared_projection(&self) -> Option<GraphPmaWriteEventProjection> {
        match self {
            Self::AppendVertex(summary) => Some(GraphPmaWriteEventProjection::BootstrapVertices(
                GraphPmaBootstrapVerticesProjection::from_single_summary(summary),
            )),
            Self::AppendVertices(summary) => Some(GraphPmaWriteEventProjection::BootstrapVertices(
                GraphPmaBootstrapVerticesProjection::from_many_summary(summary),
            )),
            Self::BootstrapEdge(summary) => Some(GraphPmaWriteEventProjection::BootstrapEdge(
                GraphPmaBootstrapEdgeProjection::from_facade_summary(summary),
            )),
            Self::BootstrapGraph(summary) => Some(GraphPmaWriteEventProjection::BootstrapGraph(
                summary.projection(),
            )),
            Self::EnsureCapacity(summary) => Some(GraphPmaWriteEventProjection::EnsureCapacity(
                GraphPmaEnsureCapacityProjection::from_summary(summary),
            )),
            Self::EnsureCapacitySegment(summary) => {
                Some(GraphPmaWriteEventProjection::EnsureCapacity(
                    GraphPmaEnsureCapacityProjection::from_segment_summary(summary),
                ))
            }
            Self::InsertEdge(summary) => Some(GraphPmaWriteEventProjection::InsertEdge(
                GraphPmaInsertEdgeProjection::from_summary(summary),
            )),
            Self::InsertEdgeSegment(summary) => Some(GraphPmaWriteEventProjection::InsertEdge(
                GraphPmaInsertEdgeProjection::from_segment_summary(summary),
            )),
            Self::MaintenanceCycle(summary) => Some(GraphPmaWriteEventProjection::MaintenanceCycle(
                GraphPmaMaintenanceCycleProjection::from_summary(summary),
            )),
            Self::MaintenanceBatch(summary) => Some(GraphPmaWriteEventProjection::MaintenanceBatch(
                GraphPmaMaintenanceBatchProjection::from_summary(summary),
            )),
            Self::MaintenanceQueue(summary) => Some(GraphPmaWriteEventProjection::MaintenanceQueue(
                summary.clone(),
            )),
            Self::Property(summary) => {
                Some(GraphPmaWriteEventProjection::Property(summary.projection()))
            }
            Self::ReplaceEdge(_) | Self::DeleteEdge(_) => self
                .edge_projection()
                .map(GraphPmaWriteEventProjection::Edge),
        }
    }

    pub fn edge_projection(&self) -> Option<GraphPmaEdgeWriteProjection> {
        match self {
            Self::ReplaceEdge(summary) => Some(GraphPmaEdgeWriteProjection {
                operation: GraphPmaEdgeWriteOperation::ReplaceLabel,
                path: summary.mutation.0,
                refreshed: summary.refreshed.clone(),
            }),
            Self::DeleteEdge(summary) => Some(GraphPmaEdgeWriteProjection {
                operation: GraphPmaEdgeWriteOperation::Delete,
                path: summary.mutation,
                refreshed: summary.refreshed.clone(),
            }),
            _ => None,
        }
    }

    pub fn property_projection(&self) -> Option<GraphPmaPropertyWriteProjection> {
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

impl GraphPmaProductionMetrics {
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

    pub fn snapshot(&self) -> GraphPmaProductionMetricsSnapshot {
        let inner = self.inner.lock().expect("metrics lock poisoned");
        let node_total = inner
            .node_eq_scan_nanos
            .iter()
            .fold(0u128, |acc, n| acc + u128::from(*n));
        let edge_total = inner
            .edge_eq_scan_nanos
            .iter()
            .fold(0u128, |acc, n| acc + u128::from(*n));
        GraphPmaProductionMetricsSnapshot {
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

impl GraphPmaRefreshedVertices {
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

impl GraphPmaPropertyIndexMutationSummary {
    pub(crate) fn from_delta(
        delta: PropertyIndexNodeStoreDelta,
        node_store_operations: Vec<PropertyIndexNodeStoreMutationKind>,
        fallback_reasons: Vec<PropertyIndexFallbackReason>,
    ) -> Self {
        let node_store = !delta.touched_node_ids.is_empty();
        Self {
            sections: GraphPmaPropertyIndexTouchedSections {
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

impl GraphPmaPropertyMutationWriteSummary {
    /// Property mutation recorded before stable writeback (until `flush` on the graph).
    pub fn pending_from_mutation(mutation: GraphPmaPropertyIndexMutationSummary) -> Self {
        Self {
            mutation,
            flushed_sections: GraphPmaPropertyIndexTouchedSections {
                property_store: false,
                logical_index: false,
                node_store: false,
            },
            refreshed: GraphPmaRefreshedVertices::new(Vec::new(), Vec::new()),
        }
    }

    pub fn is_pending_stable_flush(&self) -> bool {
        !self.flushed_sections.property_store
            && !self.flushed_sections.logical_index
            && !self.flushed_sections.node_store
    }

    pub fn projection(&self) -> GraphPmaPropertyWriteProjection {
        GraphPmaPropertyWriteProjection {
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
        mutation: GraphPmaPropertyIndexMutationSummary,
        refreshed_forward_vertices: Vec<usize>,
        refreshed_reverse_vertices: Vec<usize>,
    ) -> Self {
        Self {
            flushed_sections: mutation.sections,
            mutation,
            refreshed: GraphPmaRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        }
    }
}

impl GraphPmaEnsureCapacityProjection {
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
            refreshed: GraphPmaRefreshedVertices::new(
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
            refreshed: GraphPmaRefreshedVertices::new(
                summary.refreshed_forward_vertices.clone(),
                summary.refreshed_reverse_vertices.clone(),
            ),
        }
    }
}

impl GraphPmaInsertEdgeProjection {
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
            refreshed: GraphPmaRefreshedVertices::new(
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
            refreshed: GraphPmaRefreshedVertices::new(
                summary.refreshed_forward_vertices.clone(),
                summary.refreshed_reverse_vertices.clone(),
            ),
        }
    }
}

impl GraphPmaMaintenanceCycleProjection {
    fn storage_from_snapshot(
        snapshot: Option<GraphMaintenanceQueueStorageSnapshot>,
    ) -> Option<GraphPmaMaintenanceQueueStorageProjection> {
        snapshot.map(|snapshot| GraphPmaMaintenanceQueueStorageProjection {
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
            refreshed: GraphPmaRefreshedVertices::new(
                summary.rebalance.refreshed_forward_vertices.clone(),
                summary.rebalance.refreshed_reverse_vertices.clone(),
            ),
            queue_storage_before: Self::storage_from_snapshot(summary.queue_storage_before),
            queue_storage_after: Self::storage_from_snapshot(summary.queue_storage_after),
        }
    }
}

impl GraphPmaMaintenanceBatchProjection {
    pub(crate) fn from_summary(summary: &GraphMaintenanceBatchWriteSummary) -> Self {
        Self {
            cycles: summary.cycles.len(),
            queue_len_before: summary.queue_len_before,
            queue_len_after: summary.queue_len_after,
            swept_forward_segments: summary.swept_forward_segments.len(),
            swept_reverse_segments: summary.swept_reverse_segments.len(),
            queue_storage_before: GraphPmaMaintenanceCycleProjection::storage_from_snapshot(
                summary.queue_storage_before,
            ),
            queue_storage_after: GraphPmaMaintenanceCycleProjection::storage_from_snapshot(
                summary.queue_storage_after,
            ),
        }
    }
}

impl GraphPmaMaintenanceQueueItemProjection {
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
