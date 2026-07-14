//! Graph store helpers and edge-alias key encoding.

use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeInlineValueProfile, EdgeLabelId, EdgeSlotIndex, EdgeTarget,
    RemoteVertexId, TaggedEdgeLabelId, VertexRef,
};
use ic_stable_lara::{
    VertexId,
    labeled::{
        BucketLabelKey as LaraLabelId, DeleteEdgeObserver, EdgeSlotMove, EdgeSlotMoveObserver,
        LabeledOrientation,
    },
    traits::CsrEdge,
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

/// Tag bit for reverse-IN alias keys so they do not collide with forward-OUT slot indices
/// on the same vertex (both CSR stores use independent slot counters).
const EDGE_ALIAS_REVERSE_IN_TAG: u32 = 1 << 31;

#[inline]
pub(crate) fn edge_alias_slot_key(slot_index: u32, reverse_in: bool) -> u32 {
    if reverse_in {
        slot_index | EDGE_ALIAS_REVERSE_IN_TAG
    } else {
        slot_index
    }
}

#[inline]
pub(super) fn edge_alias_slot_key_parts(slot_key: u32) -> (u32, bool) {
    let reverse_in = slot_key & EDGE_ALIAS_REVERSE_IN_TAG != 0;

    (slot_key & !EDGE_ALIAS_REVERSE_IN_TAG, reverse_in)
}

pub(super) struct GraphSidecarMoveObserver;

impl EdgeSlotMoveObserver for GraphSidecarMoveObserver {
    fn edge_slot_moved(
        &mut self,

        orientation: LabeledOrientation,

        vid: VertexId,

        moved: EdgeSlotMove,
    ) {
        GraphStore::move_edge_sidecars(orientation, vid, moved);
    }
}

/// Observes incremental incident-edge removal during a resumable
/// [`MaintenanceWorkItem::DeleteVertex`] purge (ADR 0021 Stage 2).
///
/// Clears each removed edge's derived sidecars (edge properties, local indexes,
/// aliases) as the purge drains them, then drops the vertex from the pending-purge
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
    fn on_delete_outgoing_edge(&mut self, source: VertexId, edge: Edge) {
        let owner = if TaggedEdgeLabelId::from_raw(edge.label_id).is_undirected() {
            canonical_undirected_owner(source, edge.neighbor_vid())
        } else {
            source
        };
        self.store.clear_edge_sidecars(EdgeHandle {
            owner_vertex_id: owner,
            label_id: LaraLabelId::from_raw(edge.label_id),
            slot_index: edge.edge_slot_index.raw(),
        });
    }

    fn on_delete_incoming_edge(&mut self, _destination: VertexId, edge: Edge) {
        // Reverse out-edges are always directed (undirected edges live only in the
        // forward store), so the sidecar owner is the forward source. The reverse
        // slot is canonicalized to the forward handle inside `clear_edge_sidecars`.
        self.store.clear_edge_sidecars(EdgeHandle {
            owner_vertex_id: edge.neighbor_vid(),
            label_id: LaraLabelId::from_raw(edge.label_id),
            slot_index: edge.edge_slot_index.raw(),
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

pub(super) fn build_edge_to(target: VertexId) -> Edge {
    Edge {
        target: VertexRef::local(target),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        inline_value: gleaph_graph_kernel::entry::EdgeInlineValue::EMPTY,
    }
}

pub(super) fn build_edge_to_with_inline_value_bytes(
    target: VertexId,
    inline_value_bytes: &[u8],
) -> Edge {
    build_edge_to(target).with_inline_value_bytes(inline_value_bytes)
}

pub(super) fn build_edge_to_remote(remote_vertex_id: RemoteVertexId) -> Edge {
    Edge {
        target: VertexRef::remote_vertex(remote_vertex_id),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        inline_value: gleaph_graph_kernel::entry::EdgeInlineValue::EMPTY,
    }
}

pub(super) fn build_edge_to_remote_with_inline_value_bytes(
    remote_vertex_id: RemoteVertexId,
    inline_value_bytes: &[u8],
) -> Edge {
    build_edge_to_remote(remote_vertex_id).with_inline_value_bytes(inline_value_bytes)
}

pub(super) fn validate_edge_inline_value_bytes(
    inline_value_bytes: &[u8],
) -> Result<(), GraphStoreError> {
    if inline_value_bytes.len() > gleaph_graph_kernel::entry::MAX_EDGE_INLINE_VALUE_BYTES {
        return Err(GraphStoreError::InvalidEdgeInlineValueWidth(
            inline_value_bytes.len(),
        ));
    }
    Ok(())
}

/// Checks supported physical widths and that bytes match the router-resolved payload profile.
pub(super) fn validate_edge_inline_value_bytes_for_label(
    catalog_label: Option<EdgeLabelId>,
    inline_value_bytes: &[u8],
) -> Result<(), GraphStoreError> {
    validate_edge_inline_value_bytes(inline_value_bytes)?;
    let expected_width = catalog_label
        .map(crate::edge_inline_value_schema::lookup_edge_inline_value_profile)
        .unwrap_or_else(EdgeInlineValueProfile::no_inline_value)
        .required_byte_width();
    let expected = usize::from(expected_width);
    let actual = inline_value_bytes.len();
    if actual != expected {
        return Err(GraphStoreError::EdgeInlineValueWidthMismatch {
            label: catalog_label,
            expected,
            actual,
        });
    }
    Ok(())
}

fn edge_inline_value_bytes_match(edge: &Edge, inline_value_bytes: &[u8]) -> bool {
    edge.inline_value_bytes() == inline_value_bytes
}

pub(crate) fn edge_matches_local_neighbor(
    edge: &Edge,
    neighbor: VertexId,
    inline_value_bytes: &[u8],
) -> bool {
    edge.neighbor_vid() == neighbor && edge_inline_value_bytes_match(edge, inline_value_bytes)
}

pub(super) fn edge_matches_remote_target(
    edge: &Edge,
    remote_vertex_id: RemoteVertexId,
    inline_value_bytes: &[u8],
) -> bool {
    matches!(
        edge.edge_target(),
        Some(EdgeTarget::Remote(found)) if found == remote_vertex_id
    ) && edge_inline_value_bytes_match(edge, inline_value_bytes)
}

pub fn catalog_edge_label_from_wire(label: LaraLabelId) -> Option<EdgeLabelId> {
    if label == LaraLabelId::UNLABELED_DIRECTED || label == LaraLabelId::UNLABELED_UNDIRECTED {
        None
    } else {
        Some(EdgeLabelId::from_raw(label.label_index()))
    }
}
