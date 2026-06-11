//! Graph store helpers and edge-alias key encoding.

use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeLabelId, EdgePayloadProfile, EdgeSlotIndex, EdgeTarget,
    RemoteVertexId, TaggedEdgeLabelId, VertexRef,
};
use ic_stable_lara::{
    VertexId,
    labeled::{
        BucketLabelKey as LaraLabelId, EdgeSlotMove, EdgeSlotMoveObserver, LabeledOrientation,
    },
    traits::CsrEdge,
};

use super::GraphStore;
use super::error::GraphStoreError;

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
        GraphStore::move_edge_sidecars_for_compaction(orientation, vid, moved);
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
        payload: gleaph_graph_kernel::entry::EdgePayload::EMPTY,
    }
}

pub(super) fn build_edge_to_with_payload_bytes(target: VertexId, payload_bytes: &[u8]) -> Edge {
    build_edge_to(target).with_payload_bytes(payload_bytes)
}

pub(super) fn build_edge_to_remote(remote_vertex_id: RemoteVertexId) -> Edge {
    Edge {
        target: VertexRef::remote_vertex(remote_vertex_id),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        payload: gleaph_graph_kernel::entry::EdgePayload::EMPTY,
    }
}

pub(super) fn build_edge_to_remote_with_payload_bytes(
    remote_vertex_id: RemoteVertexId,
    payload_bytes: &[u8],
) -> Edge {
    build_edge_to_remote(remote_vertex_id).with_payload_bytes(payload_bytes)
}

pub(super) fn validate_edge_payload_bytes(payload_bytes: &[u8]) -> Result<(), GraphStoreError> {
    if payload_bytes.len() > gleaph_graph_kernel::entry::MAX_EDGE_PAYLOAD_BYTES {
        return Err(GraphStoreError::InvalidEdgePayloadWidth(
            payload_bytes.len(),
        ));
    }
    Ok(())
}

/// Checks supported physical widths and that bytes match the label's catalog payload profile width.
///
/// New catalog labels default to [`EdgePayloadProfile::no_payload`] (0 bytes). Non-zero payloads require
/// a matching profile installed at graph init via [`GraphStore::install_edge_label_payload_profile_at_init`]
/// or [`GraphStore::install_edge_label_weight_profile_at_init`].
pub(super) fn validate_edge_payload_bytes_for_label(
    store: &GraphStore,
    catalog_label: Option<EdgeLabelId>,
    payload_bytes: &[u8],
) -> Result<(), GraphStoreError> {
    validate_edge_payload_bytes(payload_bytes)?;
    let expected_width = catalog_label
        .and_then(|id| store.edge_label_payload_profile(id))
        .unwrap_or(EdgePayloadProfile::no_payload())
        .required_byte_width();
    let expected = usize::from(expected_width);
    let actual = payload_bytes.len();
    if actual != expected {
        return Err(GraphStoreError::EdgePayloadWidthMismatch {
            label: catalog_label,
            expected,
            actual,
        });
    }
    Ok(())
}

fn edge_payload_bytes_match(edge: &Edge, payload_bytes: &[u8]) -> bool {
    edge.payload_bytes() == payload_bytes
}

pub(crate) fn edge_matches_local_neighbor(
    edge: &Edge,
    neighbor: VertexId,
    payload_bytes: &[u8],
) -> bool {
    edge.neighbor_vid() == neighbor && edge_payload_bytes_match(edge, payload_bytes)
}

pub(super) fn edge_matches_remote_target(
    edge: &Edge,
    remote_vertex_id: RemoteVertexId,
    payload_bytes: &[u8],
) -> bool {
    matches!(
        edge.edge_target(),
        Some(EdgeTarget::Remote(found)) if found == remote_vertex_id
    ) && edge_payload_bytes_match(edge, payload_bytes)
}

pub fn catalog_edge_label_from_wire(label: LaraLabelId) -> Option<EdgeLabelId> {
    if label == LaraLabelId::UNLABELED_DIRECTED || label == LaraLabelId::UNLABELED_UNDIRECTED {
        None
    } else {
        Some(EdgeLabelId::from_raw(label.label_index()))
    }
}
