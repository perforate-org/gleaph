use gleaph_graph_kernel::EdgeId;

use crate::stable::Memory;
use crate::PropertyIndexNodeId;
use crate::PropertyIndexNodeStoreMutationKind;
use crate::{
    RewriteFacadeWriteEvent, RewriteGraphPma, RewriteGraphStore, RewriteGraphStoreAdapter,
    RewriteKernelOverlayGraph, RewriteOverlayWriteEvent, RewriteWriteEventProjection,
};

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

fn format_edge_write_operation(summary: &crate::RewriteEdgeWriteProjection) -> String {
    format!("{:?}@{:?}", summary.operation, summary.path)
}

fn format_edge_write_operations(summaries: &[crate::RewriteEdgeWriteProjection]) -> String {
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

fn format_refreshed_vertices(summary: &crate::RewriteRefreshedVertices) -> String {
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
        RewriteWriteEventProjection::Property(summary) => {
            format!(
                "property sections=({},{},{}) ops={} nodes=touched:{}{} alloc:{}{} freed:{}{} flushed=({},{},{}) refreshed={}",
                summary.sections.property_store,
                summary.sections.logical_index,
                summary.sections.node_store,
                format_node_store_operations(&summary.node_store_operations),
                summary.touched_node_ids.len(),
                format!(" {}", format_property_index_node_id_list(&summary.touched_node_ids)),
                summary.allocated_node_ids.len(),
                format!(" {}", format_property_index_node_id_list(&summary.allocated_node_ids)),
                summary.freed_node_ids.len(),
                format!(" {}", format_property_index_node_id_list(&summary.freed_node_ids)),
                summary.flushed_sections.property_store,
                summary.flushed_sections.logical_index,
                summary.flushed_sections.node_store,
                format_refreshed_vertices(&summary.refreshed)
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
        format_last_write_event, format_write_event_history, format_write_event_projection,
        format_write_event_report, RewriteDiagnosticsView,
    };
    use crate::property_index::PropertyIndexNodeStoreMutationKind;
    use crate::stable::VecMemory;
    use crate::{
        GraphMutationPath, RewriteEdgeWriteOperation, RewriteEdgeWriteProjection,
        RewriteEnsureCapacityProjection, RewriteGraphPma, RewriteNodeDeleteProjection,
        RewritePropertyIndexTouchedSections, RewritePropertyWriteProjection,
        RewriteRefreshedVertices, RewriteWriteEventProjection,
    };

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
    fn formatter_formats_property_write_three_leaf_repack_only() {
        let projection = RewriteWriteEventProjection::Property(RewritePropertyWriteProjection {
            sections: RewritePropertyIndexTouchedSections {
                property_store: false,
                logical_index: true,
                node_store: true,
            },
            node_store_operations: vec![PropertyIndexNodeStoreMutationKind::ThreeLeafRepack],
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
