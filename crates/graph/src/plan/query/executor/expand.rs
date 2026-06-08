//! Expand and ExpandFilter execution (CSR traversal, federation, equality index).

use std::collections::{BTreeMap, BTreeSet};

use gleaph_gql::types::EdgeDirection;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_planner::plan::{ScanValue, Str};
use gleaph_graph_kernel::entry::{Edge, EdgeDirectedness, EdgeLabelId, EdgeSlotIndex, EdgeTarget};
use gleaph_graph_kernel::federation::LogicalVertexId;
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::traits::CsrEdge;
use nohash_hasher::IntSet;

use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::bindings::EdgeBinding;
use super::{
    EdgeSequenceOrder, PlanBinding, edge_to_projected_record, resolve_scan_payload_bytes,
    row_matches_all, vertex_binding_for_projection,
};
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError, canonical_undirected_owner};
use crate::index::edge_equal;

#[derive(Clone, Copy)]
pub(crate) enum CsrOffsetFastPath {
    ForwardLabel(LaraLabelId),
    ForwardDirected,
    ForwardUndirected,
    ReverseLabel(LaraLabelId),
    ReverseDirected,
}

pub(crate) fn csr_offset_fast_path_for_expand(
    direction: EdgeDirection,
    label_id: Option<EdgeLabelId>,
    sequence_order: EdgeSequenceOrder,
) -> Option<CsrOffsetFastPath> {
    if sequence_order != EdgeSequenceOrder::Descending {
        return None;
    }
    match direction {
        EdgeDirection::PointingRight => Some(match label_id {
            Some(lid) => {
                let storage = lid.pack(EdgeDirectedness::Directed);
                CsrOffsetFastPath::ForwardLabel(LaraLabelId::from_raw(storage.raw()))
            }
            None => CsrOffsetFastPath::ForwardDirected,
        }),
        EdgeDirection::PointingLeft => Some(match label_id {
            Some(lid) => {
                let storage = lid.pack(EdgeDirectedness::Directed);
                CsrOffsetFastPath::ReverseLabel(LaraLabelId::from_raw(storage.raw()))
            }
            None => CsrOffsetFastPath::ReverseDirected,
        }),
        EdgeDirection::Undirected => Some(match label_id {
            Some(lid) => {
                let storage = lid.pack(EdgeDirectedness::Undirected);
                CsrOffsetFastPath::ForwardLabel(LaraLabelId::from_raw(storage.raw()))
            }
            None => CsrOffsetFastPath::ForwardUndirected,
        }),
        _ => None,
    }
}

fn canonical_forward_owner_for_expand(
    store: &GraphStore,
    probe_vertex_id: VertexId,
    direction: EdgeDirection,
    edge: &Edge,
) -> Result<VertexId, PlanQueryError> {
    Ok(match direction {
        EdgeDirection::PointingRight => probe_vertex_id,
        EdgeDirection::PointingLeft => store.edge_sidecar_owner_from_in_row(probe_vertex_id, edge),
        EdgeDirection::Undirected => {
            canonical_undirected_owner(probe_vertex_id, edge.neighbor_vid())
        }
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    })
}

/// Builds an expand edge binding from an edge already returned by CSR traversal.
///
/// Prefer this over [`edge_binding_for_expand`] on hot scan paths: the lookup variant
/// re-scans the label bucket to refetch the same slot.
pub(crate) fn edge_binding_for_scanned_expand(
    store: &GraphStore,
    probe_vertex_id: VertexId,
    direction: EdgeDirection,
    edge: Edge,
) -> Result<EdgeBinding, PlanQueryError> {
    edge_binding_handle_for_scanned_expand(store, probe_vertex_id, direction, &edge)
        .map(|handle| EdgeBinding::from_edge(handle, edge))
}

/// Builds an edge handle for a scanned CSR row without copying stored payload bytes.
pub(crate) fn edge_binding_handle_for_scanned_expand(
    store: &GraphStore,
    probe_vertex_id: VertexId,
    direction: EdgeDirection,
    edge: &Edge,
) -> Result<EdgeHandle, PlanQueryError> {
    let owner_vertex_id =
        canonical_forward_owner_for_expand(store, probe_vertex_id, direction, edge)?;
    Ok(EdgeHandle {
        owner_vertex_id,
        label_id: LaraLabelId::from_raw(edge.label_id),
        slot_index: edge.edge_slot_index.raw(),
    })
}

pub(crate) fn edge_binding_for_expand(
    store: &GraphStore,
    probe_vertex_id: VertexId,
    direction: EdgeDirection,
    edge: Edge,
) -> Result<EdgeBinding, PlanQueryError> {
    let owner_vertex_id =
        canonical_forward_owner_for_expand(store, probe_vertex_id, direction, &edge)?;
    let handle = EdgeHandle {
        owner_vertex_id,
        label_id: LaraLabelId::from_raw(edge.label_id),
        slot_index: edge.edge_slot_index.raw(),
    };
    let record = store
        .find_outgoing_edge_record(handle)
        .map_err(PlanQueryError::from)?
        .unwrap_or_else(|| edge.clone());
    Ok(EdgeBinding::from_edge(handle, record))
}

fn push_expand_candidate(
    out: &mut Vec<(ExpandDst, EdgeBinding)>,
    store: &GraphStore,
    probe_vertex_id: VertexId,
    direction: EdgeDirection,
    edge_dst: ExpandDst,
    edge: Edge,
) -> Result<(), PlanQueryError> {
    out.push((
        edge_dst,
        edge_binding_for_scanned_expand(store, probe_vertex_id, direction, edge)?,
    ));
    Ok(())
}

fn push_scanned_value_expand_candidate(
    out: &mut Vec<(ExpandDst, EdgeBinding)>,
    store: &GraphStore,
    probe_vertex_id: VertexId,
    direction: EdgeDirection,
    edge_dst: ExpandDst,
    edge: Edge,
) -> Result<(), PlanQueryError> {
    out.push((
        edge_dst,
        edge_binding_for_scanned_expand(store, probe_vertex_id, direction, edge)?,
    ));
    Ok(())
}

pub(crate) fn expand_accepts_remote_dst(
    dst_only_prefilter: bool,
    dst_property_projection: Option<&[Str]>,
) -> bool {
    !dst_only_prefilter && !dst_property_projection.is_some_and(|props| !props.is_empty())
}

pub(crate) fn visit_csr_expand_fast_path<Visit>(
    store: &GraphStore,
    src_id: VertexId,
    fast_path: CsrOffsetFastPath,
    offset_remaining: &mut usize,
    visit: Visit,
) -> Result<Result<bool, PlanQueryError>, GraphStoreError>
where
    Visit: FnMut(Edge) -> Result<bool, PlanQueryError>,
{
    match fast_path {
        CsrOffsetFastPath::ForwardLabel(label) => {
            store.skip_then_visit_each_out_edge_for_label(src_id, label, offset_remaining, visit)
        }
        CsrOffsetFastPath::ForwardDirected => {
            store.skip_then_visit_each_directed_out_edge(src_id, offset_remaining, visit)
        }
        CsrOffsetFastPath::ForwardUndirected => {
            store.skip_then_visit_each_undirected_edge(src_id, offset_remaining, visit)
        }
        CsrOffsetFastPath::ReverseLabel(label) => {
            store.skip_then_visit_each_in_edge_for_label(src_id, label, offset_remaining, visit)
        }
        CsrOffsetFastPath::ReverseDirected => {
            store.skip_then_visit_each_directed_in_edge(src_id, offset_remaining, visit)
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExpandDst {
    Local(VertexId),
    Remote(LogicalVertexId),
}

impl ExpandDst {
    pub(crate) fn from_edge(
        store: &GraphStore,
        edge: &Edge,
    ) -> Result<Option<Self>, PlanQueryError> {
        match edge.edge_target() {
            Some(EdgeTarget::Local(vertex_id)) => Ok(Some(Self::Local(vertex_id))),
            Some(EdgeTarget::Remote(remote_ref)) => {
                let logical = store
                    .logical_vertex_for_remote_ref(remote_ref)
                    .ok_or_else(|| PlanQueryError::MissingBinding {
                        variable: format!("remote ref {}", remote_ref.raw()),
                    })?;
                Ok(Some(Self::Remote(logical)))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn requires_local_vertex_data(self) -> bool {
        matches!(self, Self::Local(_))
    }
}

fn expand_dst_binding(
    store: &GraphStore,
    dst: ExpandDst,
    dst_property_projection: Option<&[Str]>,
) -> Result<PlanBinding, PlanQueryError> {
    match dst {
        ExpandDst::Local(vertex_id) => {
            vertex_binding_for_projection(store, vertex_id, dst_property_projection)
        }
        ExpandDst::Remote(logical_vertex_id) => {
            if dst_property_projection.is_some_and(|props| !props.is_empty()) {
                return Err(PlanQueryError::InvalidExpressionValue {
                    expression: "property projection on remote vertex binding".into(),
                });
            }
            Ok(PlanBinding::RemoteVertex(logical_vertex_id))
        }
    }
}

pub(crate) fn build_expanded_row(
    arena: Option<&mut super::super::arena::QueryArena>,
    store: &GraphStore,
    row: &PlanRow,
    edge_key: Option<&str>,
    dst_key: &str,
    dst: ExpandDst,
    edge_binding: EdgeBinding,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
) -> Result<PlanRow, PlanQueryError> {
    let dst_binding = expand_dst_binding(store, dst, dst_property_projection)?;
    let expanded = if let Some(edge_key) = edge_key {
        let edge_binding = if edge_property_projection.is_some_and(|props| !props.is_empty()) {
            PlanBinding::Value(edge_to_projected_record(
                store,
                edge_binding,
                edge_property_projection.unwrap(),
            )?)
        } else {
            PlanBinding::Edge(edge_binding)
        };
        match arena {
            Some(arena) => {
                row.fork_with_arena(arena, [(edge_key, edge_binding), (dst_key, dst_binding)])
            }
            None => row.fork([(edge_key, edge_binding), (dst_key, dst_binding)]),
        }
    } else {
        match arena {
            Some(arena) => row.fork_with_arena(arena, [(dst_key, dst_binding)]),
            None => row.fork([(dst_key, dst_binding)]),
        }
    };
    Ok(expanded)
}

fn edge_matches_indexed_equality(
    store: &GraphStore,
    probe_vertex_id: VertexId,
    direction: EdgeDirection,
    label_id: LaraLabelId,
    edge_slot_index: EdgeSlotIndex,
    edge: &Edge,
    property: &str,
    scan_value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<bool, PlanQueryError> {
    let Some(property_id) = store.property_id(property) else {
        return Ok(false);
    };
    let Some(expected) = resolve_scan_payload_bytes(scan_value, parameters)? else {
        return Ok(false);
    };
    let owner_vertex_id =
        canonical_forward_owner_for_expand(store, probe_vertex_id, direction, edge)?;
    let handle = EdgeHandle {
        owner_vertex_id,
        label_id,
        slot_index: edge_slot_index.raw(),
    };
    let Some(actual) = store.edge_property(handle, property_id) else {
        return Ok(false);
    };
    let actual_bytes =
        value_to_index_key_bytes(&actual).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "indexed edge equality value encoding".to_owned(),
        })?;
    Ok(actual_bytes == Some(expected))
}

pub(crate) enum EdgeEqualityStreamFilter {
    None,
    NoMatches,
    /// Fast path when all postings share one label: `(owner, slot)` keys in an [`IntSet`].
    IndexedSingleLabel {
        label_id: u16,
        slots: IntSet<u64>,
    },
    /// Fallback when postings span multiple labels.
    IndexedMultiLabel(BTreeSet<(u32, u16, u32)>),
    StoreLookup {
        property_id: gleaph_graph_kernel::entry::PropertyId,
        expected: Vec<u8>,
    },
}

#[inline]
fn equality_index_slot_key(owner: VertexId, slot_index: u32) -> u64 {
    u64::from(u32::from(owner)) << 32 | u64::from(slot_index)
}

pub(crate) fn edge_equality_stream_filter(
    store: &GraphStore,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    parameters: &BTreeMap<String, Value>,
) -> Result<EdgeEqualityStreamFilter, PlanQueryError> {
    let Some((property, scan_value)) = indexed_edge_equality else {
        return Ok(EdgeEqualityStreamFilter::None);
    };
    let Some(property_id) = store.property_id(property.as_ref()) else {
        return Ok(EdgeEqualityStreamFilter::NoMatches);
    };
    let Some(expected) = resolve_scan_payload_bytes(scan_value, parameters)? else {
        return Ok(EdgeEqualityStreamFilter::NoMatches);
    };
    let Some(postings) = edge_equal::lookup_equal(property_id, &expected) else {
        return Ok(EdgeEqualityStreamFilter::StoreLookup {
            property_id,
            expected,
        });
    };
    let mut labels = IntSet::default();
    let mut slots = IntSet::default();
    for posting in &postings {
        labels.insert(posting.label_id);
        slots.insert(equality_index_slot_key(
            posting.owner_vertex_id,
            posting.slot_index,
        ));
    }
    if labels.len() == 1 {
        let label_id = *labels.iter().next().expect("non-empty labels");
        Ok(EdgeEqualityStreamFilter::IndexedSingleLabel { label_id, slots })
    } else {
        let mut heterogeneous = BTreeSet::new();
        for posting in &postings {
            heterogeneous.insert((
                u32::from(posting.owner_vertex_id),
                posting.label_id,
                posting.slot_index,
            ));
        }
        Ok(EdgeEqualityStreamFilter::IndexedMultiLabel(heterogeneous))
    }
}

pub(crate) fn edge_matches_stream_filter(
    store: &GraphStore,
    filter: &EdgeEqualityStreamFilter,
    direction: EdgeDirection,
    owner_vertex_id: VertexId,
    label_id: LaraLabelId,
    edge_slot_index: EdgeSlotIndex,
) -> Result<bool, PlanQueryError> {
    match filter {
        EdgeEqualityStreamFilter::None => Ok(true),
        EdgeEqualityStreamFilter::NoMatches => Ok(false),
        EdgeEqualityStreamFilter::IndexedSingleLabel {
            label_id: expected,
            slots,
        } => {
            if label_id.raw() != *expected {
                return Ok(false);
            }
            let key = equality_index_slot_key(owner_vertex_id, edge_slot_index.raw());
            if slots.contains(&key) {
                return Ok(true);
            }
            if matches!(
                direction,
                EdgeDirection::PointingLeft | EdgeDirection::Undirected
            ) {
                let canonical = store.canonical_edge_handle(EdgeHandle {
                    owner_vertex_id,
                    label_id,
                    slot_index: edge_slot_index.raw(),
                });
                return Ok(slots.contains(&equality_index_slot_key(
                    canonical.owner_vertex_id,
                    canonical.slot_index,
                )));
            }
            Ok(false)
        }
        EdgeEqualityStreamFilter::IndexedMultiLabel(slots) => {
            if slots.contains(&(
                u32::from(owner_vertex_id),
                label_id.raw(),
                edge_slot_index.raw(),
            )) {
                return Ok(true);
            }
            if matches!(
                direction,
                EdgeDirection::PointingLeft | EdgeDirection::Undirected
            ) {
                let canonical = store.canonical_edge_handle(EdgeHandle {
                    owner_vertex_id,
                    label_id,
                    slot_index: edge_slot_index.raw(),
                });
                return Ok(slots.contains(&(
                    u32::from(canonical.owner_vertex_id),
                    canonical.label_id.raw(),
                    canonical.slot_index,
                )));
            }
            Ok(false)
        }
        EdgeEqualityStreamFilter::StoreLookup {
            property_id,
            expected,
        } => {
            let handle = EdgeHandle {
                owner_vertex_id,
                label_id,
                slot_index: edge_slot_index.raw(),
            };
            let Some(actual) = store.edge_property(handle, *property_id) else {
                return Ok(false);
            };
            let actual_bytes = value_to_index_key_bytes(&actual).map_err(|_| {
                PlanQueryError::InvalidExpressionValue {
                    expression: "indexed edge equality value encoding".to_owned(),
                }
            })?;
            Ok(actual_bytes.as_deref() == Some(expected.as_slice()))
        }
    }
}

#[allow(clippy::too_many_arguments)]
mod candidates;
mod predicates;

pub(crate) use candidates::{ExpandCandidate, expand_candidates_into};

#[cfg(test)]
mod tests;

mod execute;
pub(crate) use execute::{execute_expand, expand_dst_matches_prebound_vertex};
