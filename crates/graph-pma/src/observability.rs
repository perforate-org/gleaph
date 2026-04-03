use gleaph_graph_kernel::{EdgeId, NodeId};

use crate::facade::{
    PropertyIndexFallbackReason, RewriteEdgeWriteProjection, RewriteFacadeWriteEvent,
    RewriteGraphPma, RewriteGraphStore, RewriteGraphStoreAdapter,
    RewriteMaintenanceQueueItemProjection, RewriteMaintenanceQueueStorageProjection,
    RewriteRefreshedVertices,
    RewriteWriteEventProjection,
};
use crate::integration::{RewriteKernelOverlayGraph, RewriteOverlayWriteEvent};
use crate::property_index::{PropertyIndexNodeId, PropertyIndexNodeStoreMutationKind};
use crate::stable::Memory;

/// Small shared diagnostics boundary over rewrite observability surfaces.
///
/// This lets upper layers treat facade-style and overlay-style callers
/// uniformly when they only need shared write-event projections plus
/// formatted diagnostics strings.
pub trait RewriteDiagnosticsView {
    /// Returns the recent write history projected onto the shared event vocabulary.
    fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection>;

    /// Returns the recent projected write history formatted as diagnostics lines.
    fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(&self.shared_write_history())
    }

    /// Returns the most recent projected write event formatted as one diagnostics line.
    fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(&self.shared_write_history())
    }

    /// Returns the recent projected write history as one newline-joined diagnostics report.
    fn debug_report(&self) -> String {
        format_write_event_report(&self.shared_write_history())
    }
}

fn format_node_store_operation(kind: PropertyIndexNodeStoreMutationKind) -> &'static str {
    match kind {
        PropertyIndexNodeStoreMutationKind::LocalUpdate => "local-update",
        PropertyIndexNodeStoreMutationKind::Redistribute => "redistribute",
        PropertyIndexNodeStoreMutationKind::ThreeLeafRepack => "three-leaf-repack",
        PropertyIndexNodeStoreMutationKind::Split => "split",
        PropertyIndexNodeStoreMutationKind::Merge => "merge",
        PropertyIndexNodeStoreMutationKind::Collapse => "collapse",
        PropertyIndexNodeStoreMutationKind::Rebuild => "rebuild",
    }
}

fn format_node_store_operations(operations: &[PropertyIndexNodeStoreMutationKind]) -> String {
    if operations.is_empty() {
        return "none".to_owned();
    }
    operations
        .iter()
        .map(|kind| format_node_store_operation(*kind))
        .collect::<Vec<_>>()
        .join("|")
}

fn format_fallback_reason(reason: PropertyIndexFallbackReason) -> &'static str {
    match reason {
        PropertyIndexFallbackReason::NodeUpsertLocalUnavailable => "node-upsert-local-unavailable",
        PropertyIndexFallbackReason::NodeRemoveLocalUnavailable => "node-remove-local-unavailable",
        PropertyIndexFallbackReason::EdgeUpsertLocalUnavailable => "edge-upsert-local-unavailable",
        PropertyIndexFallbackReason::EdgeRemoveLocalUnavailable => "edge-remove-local-unavailable",
    }
}

fn format_fallback_reasons(reasons: &[PropertyIndexFallbackReason]) -> String {
    if reasons.is_empty() {
        return "none".to_owned();
    }
    reasons
        .iter()
        .map(|reason| format_fallback_reason(*reason))
        .collect::<Vec<_>>()
        .join("|")
}

fn format_edge_write_operation(summary: &RewriteEdgeWriteProjection) -> String {
    format!("{:?}@{:?}", summary.operation, summary.path)
}

fn format_edge_write_operations(summaries: &[RewriteEdgeWriteProjection]) -> String {
    if summaries.is_empty() {
        return "none".to_owned();
    }
    summaries
        .iter()
        .map(format_edge_write_operation)
        .collect::<Vec<_>>()
        .join("|")
}

fn format_edge_id_list(edge_ids: &[EdgeId]) -> String {
    if edge_ids.is_empty() {
        return "[]".to_owned();
    }
    let ids = edge_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{ids}]")
}

fn format_property_index_node_id_list(node_ids: &[PropertyIndexNodeId]) -> String {
    if node_ids.is_empty() {
        return "[]".to_owned();
    }
    let ids = node_ids
        .iter()
        .map(|id| id.0.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("[{ids}]")
}

fn format_usize_list(values: &[usize]) -> String {
    if values.is_empty() {
        return "[]".to_owned();
    }
    let joined = values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{joined}]")
}

fn format_refreshed_vertices(summary: &RewriteRefreshedVertices) -> String {
    format!(
        "({},{}) fwd={} rev={}",
        summary.forward.len(),
        summary.reverse.len(),
        format_usize_list(&summary.forward),
        format_usize_list(&summary.reverse),
    )
}

/// Formats one shared write-event projection as a compact diagnostics string.
pub fn format_write_event_projection(event: &RewriteWriteEventProjection) -> String {
    match event {
        RewriteWriteEventProjection::BootstrapVertices(summary) => {
            format!(
                "bootstrap-vertices ordinals={} refreshed={}",
                summary.ordinals.len(),
                format_refreshed_vertices(&summary.refreshed)
            )
        }
        RewriteWriteEventProjection::BootstrapEdge(summary) => {
            format!(
                "bootstrap-edge path={:?} refreshed={}",
                summary.path,
                format_refreshed_vertices(&summary.refreshed)
            )
        }
        RewriteWriteEventProjection::BootstrapGraph(summary) => {
            format!(
                "bootstrap-graph vertices={} edges={} refreshed={}",
                summary.vertex_ordinals.len(),
                summary.locators.len(),
                format_refreshed_vertices(&summary.refreshed)
            )
        }
        RewriteWriteEventProjection::EnsureCapacity(summary) => {
            format!(
                "ensure-capacity rebalanced={} displacement=({}, {}) refreshed={}",
                summary.rebalanced,
                summary.total_displacement,
                summary.max_displacement,
                format_refreshed_vertices(&summary.refreshed)
            )
        }
        RewriteWriteEventProjection::InsertEdge(summary) => {
            format!(
                "insert-edge inserted={} path={:?} rebalanced={} displacement=({}, {}) refreshed={}",
                summary.inserted,
                summary.path,
                summary.rebalanced,
                summary.total_displacement,
                summary.max_displacement,
                format_refreshed_vertices(&summary.refreshed)
            )
        }
        RewriteWriteEventProjection::MaintenanceCycle(summary) => {
            format!(
                "maintenance-cycle vertex={} ordinal={} window=({}, {}) priority={} recent=({:?}, {}) score=direct:{} window:{} tomb:{} window_total_base_slots:{} displacement=({}, {}) queue=({:?}->{:?}) refreshed={}",
                NodeId::from(summary.vertex_ref),
                summary.ordinal,
                summary.window_start_ordinal,
                summary.window_end_ordinal_exclusive,
                summary.priority_score,
                summary.last_maintenance_epoch,
                summary.recent_maintenance_penalty,
                summary.direct_overflow_total,
                summary.window_overflow_total,
                summary.reclaimable_tombstones_total,
                summary.window_total_base_slots,
                summary.total_displacement,
                summary.max_displacement,
                summary
                    .queue_storage_before
                    .as_ref()
                    .map(|storage| (storage.logical_len_bytes, storage.queue_len, storage.checksum_valid)),
                summary
                    .queue_storage_after
                    .as_ref()
                    .map(|storage| (storage.logical_len_bytes, storage.queue_len, storage.checksum_valid)),
                format_refreshed_vertices(&summary.refreshed)
            )
        }
        RewriteWriteEventProjection::MaintenanceBatch(summary) => {
            format!(
                "maintenance-batch cycles={} queue=({}, {}) maintenance_queue_storage=({:?}->{:?}) edge_segment_reclaims_fwd={} edge_segment_reclaims_rev={}",
                summary.cycles,
                summary.queue_len_before,
                summary.queue_len_after,
                summary
                    .queue_storage_before
                    .as_ref()
                    .map(|storage| (storage.logical_len_bytes, storage.queue_len, storage.checksum_valid)),
                summary
                    .queue_storage_after
                    .as_ref()
                    .map(|storage| (storage.logical_len_bytes, storage.queue_len, storage.checksum_valid)),
                summary.swept_forward_segments,
                summary.swept_reverse_segments
            )
        }
        RewriteWriteEventProjection::MaintenanceQueue(summary) => {
            format!(
                "maintenance-queue-update action={:?} queue=({}, {}) bytes={} version={}",
                summary.action,
                summary.queue_len_before,
                summary.queue_len_after,
                summary.persisted_bytes,
                summary.format_version
            )
        }
        RewriteWriteEventProjection::Property(summary) => {
            let fallback_suffix = if summary.fallback_reasons.is_empty() {
                String::new()
            } else {
                format!(
                    " fallback={}",
                    format_fallback_reasons(&summary.fallback_reasons)
                )
            };
            let touched = format_property_index_node_id_list(&summary.touched_node_ids);
            let allocated = format_property_index_node_id_list(&summary.allocated_node_ids);
            let freed = format_property_index_node_id_list(&summary.freed_node_ids);
            format!(
                "property sections=({},{},{}) ops={} nodes=touched:{} {} alloc:{} {} freed:{} {} flushed=({},{},{}) refreshed={}{}",
                summary.sections.property_store,
                summary.sections.logical_index,
                summary.sections.node_store,
                format_node_store_operations(&summary.node_store_operations),
                summary.touched_node_ids.len(),
                touched,
                summary.allocated_node_ids.len(),
                allocated,
                summary.freed_node_ids.len(),
                freed,
                summary.flushed_sections.property_store,
                summary.flushed_sections.logical_index,
                summary.flushed_sections.node_store,
                format_refreshed_vertices(&summary.refreshed),
                fallback_suffix
            )
        }
        RewriteWriteEventProjection::Edge(summary) => {
            format!(
                "edge operation={:?} path={:?} refreshed={}",
                summary.operation,
                summary.path,
                format_refreshed_vertices(&summary.refreshed)
            )
        }
        RewriteWriteEventProjection::NodeDelete(summary) => {
            format!(
                "node-delete detached={} edges={} deleted={} edge-writes={}",
                summary.detached,
                summary.deleted_edge_ids.len(),
                format_edge_id_list(&summary.deleted_edge_ids),
                format_edge_write_operations(&summary.edge_writes),
            )
        }
    }
}

/// Formats a shared write-history sequence as compact diagnostics lines.
pub fn format_write_event_history(events: &[RewriteWriteEventProjection]) -> Vec<String> {
    events.iter().map(format_write_event_projection).collect()
}

/// Formats the last shared write-event projection as a compact diagnostics string.
pub fn format_last_write_event(events: &[RewriteWriteEventProjection]) -> Option<String> {
    events.last().map(format_write_event_projection)
}

pub fn format_maintenance_queue_item(
    item: &RewriteMaintenanceQueueItemProjection,
) -> String {
    format!(
        "maintenance-queue vertex={} anchor={} window=({}, {}) priority={} recent=({:?}, {})",
        NodeId::from(item.vertex_ref),
        item.anchor_ordinal,
        item.window_start_ordinal,
        item.window_end_ordinal_exclusive,
        item.priority_score,
        item.last_maintenance_epoch,
        item.recent_maintenance_penalty,
    )
}

pub fn format_maintenance_queue(
    items: &[RewriteMaintenanceQueueItemProjection],
) -> Vec<String> {
    items.iter().map(format_maintenance_queue_item).collect()
}

pub fn format_maintenance_queue_storage(
    storage: &RewriteMaintenanceQueueStorageProjection,
) -> String {
    format!(
        "maintenance-queue-storage len={} queue={} legacy={} version={:?} checksum=({:?}, {:?}, {:?})",
        storage.logical_len_bytes,
        storage.queue_len,
        storage.legacy_format,
        storage.format_version,
        storage.stored_checksum,
        storage.computed_checksum,
        storage.checksum_valid,
    )
}

/// Formats a shared write-history sequence as one newline-joined diagnostics report.
pub fn format_write_event_report(events: &[RewriteWriteEventProjection]) -> String {
    format_write_event_history(events).join("\n")
}

/// Projects one façade write event onto the shared event vocabulary.
pub fn project_facade_write_event(
    event: &RewriteFacadeWriteEvent,
) -> Vec<RewriteWriteEventProjection> {
    event.shared_projections()
}

/// Projects façade write history onto the shared event vocabulary.
pub fn project_facade_write_history(
    events: &[RewriteFacadeWriteEvent],
) -> Vec<RewriteWriteEventProjection> {
    events.iter().flat_map(project_facade_write_event).collect()
}

/// Returns the last façade write event projected onto the shared event vocabulary.
pub fn last_projected_facade_event(
    events: &[RewriteFacadeWriteEvent],
) -> Option<RewriteWriteEventProjection> {
    events
        .iter()
        .rev()
        .find_map(RewriteFacadeWriteEvent::shared_projection)
}

/// Projects one overlay write event onto the shared event vocabulary.
pub fn project_overlay_write_event(
    event: &RewriteOverlayWriteEvent,
) -> Vec<RewriteWriteEventProjection> {
    event.shared_projections()
}

/// Projects overlay write history onto the shared event vocabulary.
pub fn project_overlay_write_history(
    events: &[RewriteOverlayWriteEvent],
) -> Vec<RewriteWriteEventProjection> {
    events
        .iter()
        .flat_map(project_overlay_write_event)
        .collect()
}

/// Returns the last overlay write event projected onto the shared event vocabulary.
pub fn last_projected_overlay_event(
    events: &[RewriteOverlayWriteEvent],
) -> Option<RewriteWriteEventProjection> {
    events
        .iter()
        .rev()
        .find_map(RewriteOverlayWriteEvent::shared_projection)
}

impl RewriteDiagnosticsView for RewriteGraphPma {
    fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection> {
        self.shared_write_history()
    }
}

impl<T> RewriteDiagnosticsView for &T
where
    T: RewriteDiagnosticsView + ?Sized,
{
    fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection> {
        (**self).shared_write_history()
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> RewriteDiagnosticsView
    for RewriteGraphStoreAdapter<'a, S, M>
{
    fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection> {
        self.shared_write_history()
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> RewriteDiagnosticsView
    for RewriteKernelOverlayGraph<'a, S, M>
{
    fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection> {
        self.shared_write_history()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RewriteDiagnosticsView, format_last_write_event, format_maintenance_queue,
        format_maintenance_queue_storage,
        format_write_event_history, format_write_event_projection, format_write_event_report,
    };
    use crate::facade::{
        RewriteEdgeWriteOperation, RewriteEdgeWriteProjection, RewriteEnsureCapacityProjection,
        RewriteGraphPma, RewriteMaintenanceBatchProjection, RewriteMaintenanceCycleProjection,
        RewriteMaintenanceQueueAction, RewriteMaintenanceQueueItemProjection,
        RewriteMaintenanceQueueProjection, RewriteMaintenanceQueueStorageProjection,
        RewriteNodeDeleteProjection,
        RewritePropertyIndexTouchedSections,
        RewritePropertyWriteProjection, RewriteRefreshedVertices, RewriteWriteEventProjection,
    };
    use crate::low_level::GraphMutationPath;
    use crate::property_index::PropertyIndexNodeStoreMutationKind;
    use crate::stable::VecMemory;

    #[test]
    fn formatter_formats_ensure_capacity_projection() {
        let projection =
            RewriteWriteEventProjection::EnsureCapacity(RewriteEnsureCapacityProjection {
                rebalanced: true,
                total_displacement: 4,
                max_displacement: 2,
                refreshed: RewriteRefreshedVertices::new(vec![0, 1], vec![3]),
            });

        assert_eq!(
            format_write_event_projection(&projection),
            "ensure-capacity rebalanced=true displacement=(4, 2) refreshed=(2,1) fwd=[0,1] rev=[3]"
        );
    }

    #[test]
    fn formatter_formats_maintenance_projections() {
        let cycle =
            RewriteWriteEventProjection::MaintenanceCycle(RewriteMaintenanceCycleProjection {
                vertex_ref: gleaph_graph_kernel::NodeId::from(7u8).into(),
                ordinal: 3,
                window_start_ordinal: 2,
                window_end_ordinal_exclusive: 5,
                priority_score: 12_345,
                last_maintenance_epoch: Some(99),
                recent_maintenance_penalty: 40_000,
                direct_overflow_total: 2,
                window_overflow_total: 5,
                reclaimable_tombstones_total: 4,
                window_total_base_slots: 9,
                total_displacement: 4,
                max_displacement: 2,
                refreshed: RewriteRefreshedVertices::new(vec![1], vec![2]),
                queue_storage_before: Some(RewriteMaintenanceQueueStorageProjection {
                    logical_len_bytes: 136,
                    queue_len: 2,
                    legacy_format: false,
                    format_version: Some(1),
                    stored_checksum: None,
                    computed_checksum: None,
                    checksum_valid: Some(true),
                }),
                queue_storage_after: Some(RewriteMaintenanceQueueStorageProjection {
                    logical_len_bytes: 80,
                    queue_len: 1,
                    legacy_format: false,
                    format_version: Some(1),
                    stored_checksum: None,
                    computed_checksum: None,
                    checksum_valid: Some(true),
                }),
            });
        let batch =
            RewriteWriteEventProjection::MaintenanceBatch(RewriteMaintenanceBatchProjection {
                cycles: 2,
                queue_len_before: 5,
                queue_len_after: 1,
                swept_forward_segments: 1,
                swept_reverse_segments: 3,
                queue_storage_before: Some(RewriteMaintenanceQueueStorageProjection {
                    logical_len_bytes: 304,
                    queue_len: 5,
                    legacy_format: false,
                    format_version: Some(1),
                    stored_checksum: None,
                    computed_checksum: None,
                    checksum_valid: Some(true),
                }),
                queue_storage_after: Some(RewriteMaintenanceQueueStorageProjection {
                    logical_len_bytes: 80,
                    queue_len: 1,
                    legacy_format: false,
                    format_version: Some(1),
                    stored_checksum: None,
                    computed_checksum: None,
                    checksum_valid: Some(true),
                }),
            });
        let queue_update =
            RewriteWriteEventProjection::MaintenanceQueue(RewriteMaintenanceQueueProjection {
                action: RewriteMaintenanceQueueAction::Refresh,
                queue_len_before: 4,
                queue_len_after: 2,
                persisted_bytes: 128,
                format_version: 1,
            });

        assert_eq!(
            format_write_event_projection(&cycle),
            "maintenance-cycle vertex=7 ordinal=3 window=(2, 5) priority=12345 recent=(Some(99), 40000) score=direct:2 window:5 tomb:4 window_total_base_slots:9 displacement=(4, 2) queue=(Some((136, 2, Some(true)))->Some((80, 1, Some(true)))) refreshed=(1,1) fwd=[1] rev=[2]"
        );
        assert_eq!(
            format_write_event_projection(&batch),
            "maintenance-batch cycles=2 queue=(5, 1) maintenance_queue_storage=(Some((304, 5, Some(true)))->Some((80, 1, Some(true)))) edge_segment_reclaims_fwd=1 edge_segment_reclaims_rev=3"
        );
        assert_eq!(
            format_write_event_projection(&queue_update),
            "maintenance-queue-update action=Refresh queue=(4, 2) bytes=128 version=1"
        );
    }

    #[test]
    fn formatter_formats_maintenance_queue() {
        let queue = vec![
            RewriteMaintenanceQueueItemProjection {
                vertex_ref: gleaph_graph_kernel::NodeId::from(7u8).into(),
                anchor_ordinal: 3,
                window_start_ordinal: 2,
                window_end_ordinal_exclusive: 5,
                priority_score: 12_345,
                last_maintenance_epoch: Some(99),
                recent_maintenance_penalty: 40_000,
            },
            RewriteMaintenanceQueueItemProjection {
                vertex_ref: gleaph_graph_kernel::NodeId::from(8u8).into(),
                anchor_ordinal: 5,
                window_start_ordinal: 5,
                window_end_ordinal_exclusive: 6,
                priority_score: 99,
                last_maintenance_epoch: None,
                recent_maintenance_penalty: 0,
            },
        ];

        assert_eq!(
            format_maintenance_queue(&queue),
            vec![
                "maintenance-queue vertex=7 anchor=3 window=(2, 5) priority=12345 recent=(Some(99), 40000)".to_owned(),
                "maintenance-queue vertex=8 anchor=5 window=(5, 6) priority=99 recent=(None, 0)".to_owned(),
            ]
        );
    }

    #[test]
    fn formatter_formats_maintenance_queue_storage() {
        let storage = RewriteMaintenanceQueueStorageProjection {
            logical_len_bytes: 136,
            queue_len: 2,
            legacy_format: false,
            format_version: Some(1),
            stored_checksum: Some(123),
            computed_checksum: Some(123),
            checksum_valid: Some(true),
        };

        assert_eq!(
            format_maintenance_queue_storage(&storage),
            "maintenance-queue-storage len=136 queue=2 legacy=false version=Some(1) checksum=(Some(123), Some(123), Some(true))"
        );
    }

    #[test]
    fn formatter_formats_history_sequence() {
        let history = vec![
            RewriteWriteEventProjection::Edge(RewriteEdgeWriteProjection {
                operation: RewriteEdgeWriteOperation::Delete,
                path: GraphMutationPath::Base,
                refreshed: RewriteRefreshedVertices::new(vec![0], vec![]),
            }),
            RewriteWriteEventProjection::NodeDelete(RewriteNodeDeleteProjection {
                detached: true,
                deleted_edge_ids: vec![7, 8],
                edge_writes: Vec::new(),
            }),
        ];

        assert_eq!(
            format_write_event_history(&history),
            vec![
                "edge operation=Delete path=Base refreshed=(1,0) fwd=[0] rev=[]".to_owned(),
                "node-delete detached=true edges=2 deleted=[7,8] edge-writes=none".to_owned(),
            ]
        );
    }

    #[test]
    fn formatter_formats_property_write_split_only() {
        let projection = RewriteWriteEventProjection::Property(RewritePropertyWriteProjection {
            sections: RewritePropertyIndexTouchedSections {
                property_store: false,
                logical_index: true,
                node_store: true,
            },
            node_store_operations: vec![PropertyIndexNodeStoreMutationKind::Split],
            fallback_reasons: Vec::new(),
            touched_node_ids: Vec::new(),
            allocated_node_ids: Vec::new(),
            freed_node_ids: Vec::new(),
            flushed_sections: RewritePropertyIndexTouchedSections {
                property_store: false,
                logical_index: true,
                node_store: true,
            },
            refreshed: RewriteRefreshedVertices::new(Vec::new(), Vec::new()),
        });
        assert_eq!(
            format_write_event_projection(&projection),
            "property sections=(false,true,true) ops=split nodes=touched:0 [] alloc:0 [] freed:0 [] flushed=(false,true,true) refreshed=(0,0) fwd=[] rev=[]"
        );
    }

    #[test]
    fn formatter_formats_property_write_three_leaf_repack_only() {
        let projection = RewriteWriteEventProjection::Property(RewritePropertyWriteProjection {
            sections: RewritePropertyIndexTouchedSections {
                property_store: false,
                logical_index: true,
                node_store: true,
            },
            node_store_operations: vec![PropertyIndexNodeStoreMutationKind::ThreeLeafRepack],
            fallback_reasons: Vec::new(),
            touched_node_ids: Vec::new(),
            allocated_node_ids: Vec::new(),
            freed_node_ids: Vec::new(),
            flushed_sections: RewritePropertyIndexTouchedSections {
                property_store: false,
                logical_index: true,
                node_store: true,
            },
            refreshed: RewriteRefreshedVertices::new(Vec::new(), Vec::new()),
        });
        assert_eq!(
            format_write_event_projection(&projection),
            "property sections=(false,true,true) ops=three-leaf-repack nodes=touched:0 [] alloc:0 [] freed:0 [] flushed=(false,true,true) refreshed=(0,0) fwd=[] rev=[]"
        );
    }

    #[test]
    fn formatter_formats_property_operations() {
        let history = vec![RewriteWriteEventProjection::Property(
            RewritePropertyWriteProjection {
                sections: RewritePropertyIndexTouchedSections {
                    property_store: true,
                    logical_index: true,
                    node_store: true,
                },
                node_store_operations: vec![
                    PropertyIndexNodeStoreMutationKind::Collapse,
                    PropertyIndexNodeStoreMutationKind::ThreeLeafRepack,
                    PropertyIndexNodeStoreMutationKind::Rebuild,
                ],
                fallback_reasons: Vec::new(),
                touched_node_ids: vec![crate::PropertyIndexNodeId(7)],
                allocated_node_ids: vec![crate::PropertyIndexNodeId(11)],
                freed_node_ids: vec![crate::PropertyIndexNodeId(5)],
                flushed_sections: RewritePropertyIndexTouchedSections {
                    property_store: true,
                    logical_index: true,
                    node_store: true,
                },
                refreshed: RewriteRefreshedVertices::new(Vec::new(), Vec::new()),
            },
        )];

        assert_eq!(
            format_write_event_history(&history),
            vec!["property sections=(true,true,true) ops=collapse|three-leaf-repack|rebuild nodes=touched:1 [7] alloc:1 [11] freed:1 [5] flushed=(true,true,true) refreshed=(0,0) fwd=[] rev=[]".to_owned()]
        );
    }

    #[test]
    fn formatter_formats_last_history_event() {
        let history = vec![
            RewriteWriteEventProjection::Edge(RewriteEdgeWriteProjection {
                operation: RewriteEdgeWriteOperation::Delete,
                path: GraphMutationPath::Base,
                refreshed: RewriteRefreshedVertices::new(vec![0], vec![]),
            }),
            RewriteWriteEventProjection::NodeDelete(RewriteNodeDeleteProjection {
                detached: true,
                deleted_edge_ids: vec![7, 8],
                edge_writes: Vec::new(),
            }),
        ];

        assert_eq!(
            format_last_write_event(&history),
            Some("node-delete detached=true edges=2 deleted=[7,8] edge-writes=none".to_owned())
        );
    }

    #[test]
    fn diagnostics_view_formats_facade_history() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");

        let _ = facade
            .append_empty_vertex_pair_and_write(&memory)
            .expect("append empty vertex");

        assert_eq!(
            RewriteDiagnosticsView::formatted_write_history(&facade),
            vec!["bootstrap-vertices ordinals=1 refreshed=(0,0) fwd=[] rev=[]".to_owned()]
        );
        assert_eq!(
            RewriteDiagnosticsView::formatted_last_write_event(&facade),
            Some("bootstrap-vertices ordinals=1 refreshed=(0,0) fwd=[] rev=[]".to_owned())
        );
        assert_eq!(
            RewriteDiagnosticsView::debug_report(&facade),
            "bootstrap-vertices ordinals=1 refreshed=(0,0) fwd=[] rev=[]"
        );
    }

    #[test]
    fn formatter_formats_multiline_report() {
        let history = vec![
            RewriteWriteEventProjection::Edge(RewriteEdgeWriteProjection {
                operation: RewriteEdgeWriteOperation::Delete,
                path: GraphMutationPath::Base,
                refreshed: RewriteRefreshedVertices::new(vec![0], vec![]),
            }),
            RewriteWriteEventProjection::NodeDelete(RewriteNodeDeleteProjection {
                detached: true,
                deleted_edge_ids: vec![7, 8],
                edge_writes: Vec::new(),
            }),
        ];

        assert_eq!(
            format_write_event_report(&history),
            "edge operation=Delete path=Base refreshed=(1,0) fwd=[0] rev=[]\nnode-delete detached=true edges=2 deleted=[7,8] edge-writes=none"
        );
    }
}
