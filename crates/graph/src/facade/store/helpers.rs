//! Graph store helpers.

use crate::facade::topology::change::RemovedEdge;
use crate::facade::topology::insert::Placement;
use gleaph_graph_kernel::entry::VertexId;
use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeInlinePropertyBytes, EdgeInlinePropertyProfile, EdgeLabelId,
    EdgeSlotIndex, EdgeTarget, EdgeWithInlinePropertyBytes, RemoteVertexId, TaggedEdgeLabelId,
    VertexRef,
};
use gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::property::dispatch_inline_index_removals;

/// Clears the sidecars of an edge a vertex delete removed (ADR 0021 Stage 2): its inline index
/// postings and its edge properties and local indexes, at its canonical half. The store has
/// already removed both halves, so a failure here cannot be returned; dispatch errors are
/// dropped as they were for the deferred purge.
pub(super) fn clear_removed_edge(store: GraphStore, removed: &RemovedEdge) {
    let canonical = removed.canonical;
    let _ = dispatch_inline_index_removals(
        canonical.owner_vertex_id,
        canonical.label_id.raw(),
        canonical.slot_index.raw(),
        &removed.value,
    );
    store.commit_clear_edge_sidecars_at_canonical(EdgeHandle {
        owner_vertex_id: canonical.owner_vertex_id,
        label_id: canonical.label_id,
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
    crate::facade::topology::counterpart::canonical_undirected_owner(a, b)
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

/// Maps the resolved ordering policy to the store's placement at the mutation boundary (ADR 0052
/// §4, ADR 0136 Decision 5): `ORDER BY INSERTION` labels append, others reuse holes. An
/// undeclared/unknown label resolves to the `Unordered` default (ADR 0052 §1).
pub(crate) fn lara_edge_placement(ordering: Option<EdgeOrderingPolicy>) -> Placement {
    match ordering {
        None | Some(EdgeOrderingPolicy::Unordered) => Placement::ReuseHoles,
        Some(EdgeOrderingPolicy::Insertion) => Placement::Append,
    }
}
