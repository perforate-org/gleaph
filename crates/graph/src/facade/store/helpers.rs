//! Graph store helpers.

use crate::facade::topology::change::{RemovedHalf, TopologyChange};
use crate::facade::topology::insert::EdgePlacementPolicy;
use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeInlinePropertyBytes, EdgeInlinePropertyProfile, EdgeLabelId,
    EdgeSlotIndex, EdgeTarget, EdgeWithInlinePropertyBytes, RemoteVertexId, TaggedEdgeLabelId,
    VertexRef,
};
use gleaph_graph_kernel::entry::{EdgeOrientation, VertexId};
use gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::facade::store::index_build_admission::trap_post_fence_commit;
use crate::property::dispatch_inline_index_removals;

/// Applies one change deferred maintenance reports while it drains
/// ([`crate::facade::topology::change::run_maintenance`]).
///
/// Runs inside the store's borrow, so it only touches thread-locals and nested shared reads of
/// the store, never writes it.
///
/// - A slot move relocates the moved edge's properties: forward (canonical) moves carry all of
///   the edge's properties and their derived index postings; reverse rows are mirrors with no
///   canonical sidecars. The store's move has already happened, so a relocation failure must trap
///   and let the IC roll the whole message back.
/// - A half removed by a resumable vertex purge (ADR 0021 Stage 2) clears the removed edge's
///   derived sidecars (edge properties and local indexes). Sidecar owner and directedness are
///   derived from the edge's bucket label, mirroring `edge_sidecar_owner_from_*` without
///   re-reading the store.
/// - A completed purge drops the vertex from the pending-purge set.
pub(super) fn apply_maintenance_change(store: GraphStore, change: TopologyChange) {
    match change {
        TopologyChange::SlotMoved {
            side,
            vertex_id,
            moved,
        } => {
            if side == EdgeOrientation::Forward {
                store
                    .relocate_edge_properties_for_move(vertex_id, moved, 0.into())
                    .unwrap_or_else(trap_post_fence_commit);
            }
        }
        TopologyChange::HalfRemoved {
            removed,
            counterpart,
        } => clear_removed_edge(store, removed, counterpart),
        TopologyChange::PurgeCompleted(vertex_id) => store.clear_vertex_pending_purge(vertex_id),
    }
}

/// Clears the sidecars of the edge a purge removed, at its canonical half: a directed edge's
/// forward half, an undirected edge's half at its canonical owner.
fn clear_removed_edge(
    store: GraphStore,
    removed: RemovedHalf<Edge>,
    counterpart: Option<RemovedHalf<Edge>>,
) {
    let canonical = if removed.label_id.is_directed() {
        match removed.orientation {
            EdgeOrientation::Forward => removed,
            EdgeOrientation::Reverse => {
                let Some(counterpart) = counterpart else {
                    // A directed self-loop is reported once from its forward row.
                    return;
                };
                counterpart
            }
        }
    } else {
        match counterpart {
            Some(counterpart)
                if canonical_undirected_owner(
                    removed.owner_vertex_id,
                    counterpart.owner_vertex_id,
                ) == removed.owner_vertex_id =>
            {
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
    store.commit_clear_edge_sidecars_at_canonical(EdgeHandle {
        owner_vertex_id: canonical.owner_vertex_id,
        label_id: TaggedEdgeLabelId::from_raw(canonical.label_id.raw()),
        slot_index: canonical.slot_index,
    });
}

pub(crate) fn edge_storage_label(
    catalog: Option<EdgeLabelId>,
    directedness: EdgeDirectedness,
) -> TaggedEdgeLabelId {
    match (catalog, directedness) {
        (None, gleaph_graph_kernel::entry::EdgeDirectedness::Directed) => {
            TaggedEdgeLabelId::UNLABELED_DIRECTED
        }
        (None, gleaph_graph_kernel::entry::EdgeDirectedness::Undirected) => {
            TaggedEdgeLabelId::UNLABELED_UNDIRECTED
        }
        (Some(catalog_id), directedness) => catalog_id.pack(directedness),
    }
}

pub(crate) fn lara_label(id: TaggedEdgeLabelId) -> TaggedEdgeLabelId {
    TaggedEdgeLabelId::from_raw(id.raw())
}

pub(super) fn wire_catalog_label(
    label: Option<EdgeLabelId>,
    directedness: EdgeDirectedness,
) -> TaggedEdgeLabelId {
    lara_label(edge_storage_label(label, directedness))
}

pub fn canonical_undirected_owner(a: VertexId, b: VertexId) -> VertexId {
    // One rule, one owner (ADR 0099): the undirected canonical endpoint is decided where the
    // counterpart is, so the graph layer does not restate the comparison.
    ic_stable_lara::labeled::bidirectional::counterpart::canonical_undirected_owner(a, b)
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

pub fn catalog_edge_label_from_wire(label: TaggedEdgeLabelId) -> Option<EdgeLabelId> {
    if label == TaggedEdgeLabelId::UNLABELED_DIRECTED
        || label == TaggedEdgeLabelId::UNLABELED_UNDIRECTED
    {
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
