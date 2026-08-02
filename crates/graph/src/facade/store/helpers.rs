//! Graph store helpers.

use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeInlinePropertyBytes, EdgeInlinePropertyProfile, EdgeLabelId,
    EdgeSlotIndex, EdgeTarget, EdgeWithInlinePropertyBytes, RemoteVertexId, TaggedEdgeLabelId,
    VertexRef,
};
use gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy;
use ic_stable_lara::{
    VertexId,
    labeled::{
        BucketLabelKey as LaraLabelId, DeleteEdgeObserver, DeletedEdge, EdgePlacementPolicy,
        EdgeSlotMove, EdgeSlotMoveObserver, LabeledOrientation,
    },
    traits::CsrEdge,
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::property::dispatch_inline_index_removals;

pub(super) struct GraphSidecarMoveObserver {
    pub(super) inline_moves: Vec<(VertexId, EdgeSlotMove)>,
}

impl EdgeSlotMoveObserver for GraphSidecarMoveObserver {
    fn edge_slot_moved(
        &mut self,

        orientation: LabeledOrientation,

        vid: VertexId,

        moved: EdgeSlotMove,
    ) {
        GraphStore::move_edge_sidecars(orientation, vid, moved);
        if orientation == LabeledOrientation::Forward {
            self.inline_moves.push((vid, moved));
        }
    }
}

/// Observes incremental incident-edge removal during a resumable
/// [`MaintenanceWorkItem::DeleteVertex`] purge (ADR 0021 Stage 2).
///
/// Clears each removed edge's derived sidecars (edge properties and local indexes)
/// as the purge drains them, then drops the vertex from the pending-purge
/// set when its purge completes. Runs inside the `GRAPH` borrow held by
/// `maintenance_with_observers`, so it only touches the edge-sidecar and
/// pending-purge thread-locals — never `GRAPH` itself. Sidecar owner and
/// directedness are derived from the edge's bucket `label_id` (set by the
/// maintenance iterator), mirroring `edge_sidecar_owner_from_*` without re-reading
/// `GRAPH`.
///
/// [`MaintenanceWorkItem::DeleteVertex`]: ic_stable_lara::labeled::MaintenanceWorkItem
pub(super) struct GraphDeleteEdgeObserver {
    pub(super) store: GraphStore,
}

impl DeleteEdgeObserver<Edge> for GraphDeleteEdgeObserver {
    fn on_delete_edge(
        &mut self,
        removed: DeletedEdge<Edge>,
        counterpart: Option<DeletedEdge<Edge>>,
    ) {
        let canonical = if removed.label_id.is_directed() {
            match removed.orientation {
                LabeledOrientation::Forward => removed,
                LabeledOrientation::Reverse => {
                    let Some(counterpart) = counterpart else {
                        // A directed self-loop is reported once from its forward row.
                        return;
                    };
                    counterpart
                }
            }
        } else {
            match counterpart {
                Some(counterpart) if removed.owner_vertex_id >= counterpart.owner_vertex_id => {
                    removed
                }
                Some(counterpart) => counterpart,
                None if removed.edge.neighbor_vid() == removed.owner_vertex_id => removed,
                None => return,
            }
        };
        let _ = dispatch_inline_index_removals(
            canonical.owner_vertex_id,
            canonical.label_id.raw(),
            canonical.slot_index.raw(),
            canonical.edge.edge_inline_property_bytes(),
        );
        self.store
            .commit_clear_edge_sidecars_at_canonical(EdgeHandle {
                owner_vertex_id: canonical.owner_vertex_id,
                label_id: LaraLabelId::from_raw(canonical.label_id.raw()),
                slot_index: canonical.slot_index,
            });
    }

    fn on_vertex_purge_completed(&mut self, vid: VertexId) {
        self.store.clear_vertex_pending_purge(vid);
    }
}

pub(crate) fn edge_storage_label(
    catalog: Option<EdgeLabelId>,
    undirected: bool,
) -> TaggedEdgeLabelId {
    match catalog {
        None => {
            if undirected {
                TaggedEdgeLabelId::UNLABELED_UNDIRECTED
            } else {
                TaggedEdgeLabelId::UNLABELED_DIRECTED
            }
        }

        Some(catalog_id) => {
            if undirected {
                catalog_id.pack(EdgeDirectedness::Undirected)
            } else {
                catalog_id.pack(EdgeDirectedness::Directed)
            }
        }
    }
}

pub(crate) fn lara_label(id: TaggedEdgeLabelId) -> LaraLabelId {
    LaraLabelId::from_raw(id.raw())
}

pub(super) fn wire_catalog_label(
    label: Option<EdgeLabelId>,
    directedness: EdgeDirectedness,
) -> LaraLabelId {
    lara_label(edge_storage_label(
        label,
        matches!(directedness, EdgeDirectedness::Undirected),
    ))
}

pub fn canonical_undirected_owner(a: VertexId, b: VertexId) -> VertexId {
    if u32::from(a) >= u32::from(b) { a } else { b }
}

pub(crate) fn build_edge_to(target: VertexId) -> Edge {
    Edge {
        target: VertexRef::local(target),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        inline_property: EdgeInlinePropertyBytes::EMPTY,
    }
}

pub(crate) fn build_edge_to_with_inline_property_bytes(
    target: VertexId,
    inline_property_bytes: &[u8],
) -> EdgeWithInlinePropertyBytes {
    EdgeWithInlinePropertyBytes::with_inline_property_bytes(
        build_edge_to(target),
        inline_property_bytes,
    )
}

pub(super) fn build_edge_to_remote(remote_vertex_id: RemoteVertexId) -> Edge {
    Edge {
        target: VertexRef::remote_vertex(remote_vertex_id),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        inline_property: EdgeInlinePropertyBytes::EMPTY,
    }
}

pub(super) fn build_edge_to_remote_with_inline_property_bytes(
    remote_vertex_id: RemoteVertexId,
    inline_property_bytes: &[u8],
) -> EdgeWithInlinePropertyBytes {
    EdgeWithInlinePropertyBytes::with_inline_property_bytes(
        build_edge_to_remote(remote_vertex_id),
        inline_property_bytes,
    )
}

pub(super) fn validate_edge_inline_property_bytes(
    inline_property_bytes: &[u8],
) -> Result<(), GraphStoreError> {
    if inline_property_bytes.len() > gleaph_graph_kernel::entry::MAX_EDGE_INLINE_PROPERTY_BYTES {
        return Err(GraphStoreError::InvalidEdgeInlinePropertyBytesWidth(
            inline_property_bytes.len(),
        ));
    }
    Ok(())
}

/// Checks supported physical widths and that bytes match the router-resolved inline property profile.
pub(super) fn validate_edge_inline_property_bytes_for_label(
    catalog_label: Option<EdgeLabelId>,
    inline_property_bytes: &[u8],
) -> Result<(), GraphStoreError> {
    validate_edge_inline_property_bytes(inline_property_bytes)?;
    let expected_width = catalog_label
        .map(crate::edge_inline_property_schema::lookup_edge_inline_property_profile)
        .unwrap_or_else(EdgeInlinePropertyProfile::no_inline_property)
        .required_byte_width();
    let expected = usize::from(expected_width);
    let actual = inline_property_bytes.len();
    if actual != expected {
        return Err(GraphStoreError::EdgeInlinePropertyBytesWidthMismatch {
            label: catalog_label,
            expected,
            actual,
        });
    }
    Ok(())
}

fn edge_inline_property_bytes_match(edge: &Edge, inline_property_bytes: &[u8]) -> bool {
    // Topology-only Edge no longer carries inline property bytes; comparison for
    // mutation-time identity now relies on the caller-supplied bytes.
    edge.edge_inline_property_bytes() == inline_property_bytes
}

/// Compares the inline property bytes stored on a mutation-time edge wrapper with a slice.
pub(super) fn inline_property_bytes_match(
    edge: &EdgeWithInlinePropertyBytes,
    inline_property_bytes: &[u8],
) -> bool {
    edge.inline_property_bytes() == inline_property_bytes
}

pub(crate) fn edge_matches_local_neighbor(
    edge: &Edge,
    neighbor: VertexId,
    inline_property_bytes: &[u8],
) -> bool {
    edge.neighbor_vid() == neighbor && edge_inline_property_bytes_match(edge, inline_property_bytes)
}

pub(super) fn edge_matches_remote_target(
    edge: &Edge,
    remote_vertex_id: RemoteVertexId,
    inline_property_bytes: &[u8],
) -> bool {
    matches!(
        edge.edge_target(),
        Some(EdgeTarget::Remote(found)) if found == remote_vertex_id
    ) && edge_inline_property_bytes_match(edge, inline_property_bytes)
}

pub fn catalog_edge_label_from_wire(label: LaraLabelId) -> Option<EdgeLabelId> {
    if label == LaraLabelId::UNLABELED_DIRECTED || label == LaraLabelId::UNLABELED_UNDIRECTED {
        None
    } else {
        Some(EdgeLabelId::from_raw(label.label_index()))
    }
}

/// Maps the resolved ordering policy to the storage-owned LARA placement enum
/// at the mutation boundary (ADR 0052 §4). An undeclared/unknown label resolves
/// to the `Unordered` default (ADR 0052 §1).
pub(crate) fn lara_edge_placement(ordering: Option<EdgeOrderingPolicy>) -> EdgePlacementPolicy {
    match ordering {
        None | Some(EdgeOrderingPolicy::Unordered) => EdgePlacementPolicy::Unordered,
        Some(EdgeOrderingPolicy::Insertion) => EdgePlacementPolicy::Insertion,
    }
}
