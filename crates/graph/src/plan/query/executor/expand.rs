//! Expand and ExpandFilter execution (CSR traversal, federation, equality index).

use std::collections::{BTreeMap, BTreeSet};

use candid::Principal;
use gleaph_gql::ast::{CmpOp, Expr};
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_planner::plan::{
    EdgeValuePredicate, EdgeVectorMetric as PlanEdgeVectorMetric, EdgeVectorPredicate, ScanValue,
    Str,
};
use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeLabelId, EdgeSlotIndex, EdgeTarget, EdgeValueEncoding,
    EdgeValueProfile, PreparedWeightDecoder,
};
use gleaph_graph_kernel::federation::{
    FederatedExpandArgs, FederatedExpandDirection, FederatedExpandNeighbor, LogicalVertexId,
};
use half::f16;
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::LabeledEdgeValueBatchScratch;
use ic_stable_lara::traits::CsrEdge;
use nohash_hasher::IntSet;

use super::super::edge_value_batch_kernel::PreparedEdgeValueBatchKernel;
use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::bindings::{
    EdgeBinding, edge_binding_for_federated_expand_hit, federated_expand_label_id_raw,
};
use super::context::{ExecuteCtx, QueryExprEvaluator};
use super::{
    EdgeSequenceOrder, PlanBinding, dst_filter_is_dst_vertex_only, edge_to_projected_record,
    federation_routing, resolve_federated_traversal_vertex, resolve_scan_value_bytes,
    row_matches_all, vertex_binding_for_projection, vertex_binding_for_traversal,
    vertex_row_matches_dst_filters,
};
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError, canonical_undirected_owner};
use crate::index::edge_equal;
use crate::plan::query::edge_vector_kernel::{
    EdgeVectorMetric as KernelEdgeVectorMetric, PreparedEdgeVectorKernel,
};

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
        .unwrap_or(edge);
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
        edge_binding_for_expand(store, probe_vertex_id, direction, edge)?,
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
    let owner_vertex_id =
        canonical_forward_owner_for_expand(store, probe_vertex_id, direction, &edge)?;
    let handle = EdgeHandle {
        owner_vertex_id,
        label_id: LaraLabelId::from_raw(edge.label_id),
        slot_index: edge.edge_slot_index.raw(),
    };
    out.push((edge_dst, EdgeBinding::from_edge(handle, edge)));
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
    let Some(expected) = resolve_scan_value_bytes(scan_value, parameters)? else {
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
    let Some(expected) = resolve_scan_value_bytes(scan_value, parameters)? else {
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
fn expand_rows_from_federated_expand_hits(
    store: &GraphStore,
    row: &PlanRow,
    hits: &[FederatedExpandNeighbor],
    dst: &str,
    edge: &str,
    emit_edge_binding: bool,
    dst_property_projection: Option<&[Str]>,
    dst_filter: &[Expr],
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let routing = federation_routing(store)?;
    let dst_only_prefilter = dst_filter_is_dst_vertex_only(dst_filter, dst);
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        let dst_binding = if hit.shard_id == routing.shard_id {
            expand_dst_binding(
                store,
                ExpandDst::Local(VertexId::from(hit.neighbor_local_vertex_id)),
                dst_property_projection,
            )?
        } else {
            if dst_property_projection.is_some_and(|props| !props.is_empty()) {
                return Err(PlanQueryError::InvalidExpressionValue {
                    expression: "property projection on remote vertex binding".into(),
                });
            }
            PlanBinding::RemoteVertex(hit.neighbor_logical_vertex_id)
        };

        if let PlanBinding::Vertex(dst_id) = &dst_binding
            && dst_only_prefilter
            && !vertex_row_matches_dst_filters(
                store,
                parameters,
                &Str::from(dst),
                *dst_id,
                dst_filter,
                caller,
                gleaph_weight_decoders,
            )?
        {
            continue;
        }

        let expanded = if let Some(edge_key) = edge_key.as_ref() {
            let edge_binding = PlanBinding::Edge(edge_binding_for_federated_expand_hit(
                store,
                hit,
                routing.shard_id,
            )?);
            row.fork([(dst, dst_binding), (edge_key.as_str(), edge_binding)])
        } else {
            row.fork([(dst, dst_binding)])
        };
        if !dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
            continue;
        }
        out.push(expanded);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_expand(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&Str>,
    sequence_order: EdgeSequenceOrder,
    dst_filter: &[Expr],
    emit_edge_binding: bool,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_value_predicate: Option<&EdgeValuePredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let store = ctx.store;
    let parameters = ctx.parameters;
    let caller = ctx.caller();
    let gleaph_weight_decoders = ctx.gleaph_weight_decoders;
    let label_id = label.and_then(|label| store.edge_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let evaluator = ctx.expr_evaluator(None);
    let dst_only_prefilter = dst_filter_is_dst_vertex_only(dst_filter, dst.as_ref());
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let dst_key = dst.to_string();
    let csr_expand_fast_path = (edge_value_predicate.is_none() && edge_vector_predicate.is_none())
        .then(|| csr_offset_fast_path_for_expand(direction, label_id, sequence_order))
        .flatten();
    let prepared_vector_dst_only_predicate = prepare_vector_dst_only_expand_predicate(
        store,
        label_id,
        direction,
        emit_edge_binding,
        indexed_edge_equality,
        edge_value_predicate,
        edge_vector_predicate,
        edge_property_projection,
        parameters,
    )?;
    let edge_equality_filter = if csr_expand_fast_path.is_some() {
        let filter = edge_equality_stream_filter(store, indexed_edge_equality, parameters)?;
        if matches!(filter, EdgeEqualityStreamFilter::NoMatches) {
            return Ok(Vec::new());
        }
        Some(filter)
    } else {
        None
    };
    let mut out = Vec::with_capacity(rows.len());
    let mut candidates = Vec::new();
    let mut vector_batch_scratch = LabeledEdgeValueBatchScratch::default();
    let mut vector_matches = Vec::new();
    for row in rows {
        if matches!(direction, EdgeDirection::PointingLeft)
            && let Some(PlanBinding::RemoteVertex(logical)) = row.get(src.as_ref())
            && matches!(
                resolve_federated_traversal_vertex(store, *logical, Some(direction)).await,
                Err(PlanQueryError::UnsupportedOp(_))
            )
        {
            let label_id_raw = federated_expand_label_id_raw(label_id, direction);
            let hits = crate::facade::federation_expand::federated_expand_coordinator(
                store,
                FederatedExpandArgs {
                    logical_vertex_id: *logical,
                    direction: FederatedExpandDirection::Incoming,
                    label_id_raw,
                },
            )
            .await
            .map_err(|e| PlanQueryError::FederatedIndexCall {
                op: "federated_expand",
                detail: e.to_string(),
            })?;
            out.extend(expand_rows_from_federated_expand_hits(
                store,
                &row,
                &hits,
                dst.as_ref(),
                edge.as_ref(),
                emit_edge_binding,
                dst_property_projection,
                dst_filter,
                parameters,
                caller,
                gleaph_weight_decoders,
                &evaluator,
            )?);
            continue;
        }

        if matches!(direction, EdgeDirection::PointingRight)
            && let Some(PlanBinding::RemoteVertex(logical)) = row.get(src.as_ref())
            && matches!(
                resolve_federated_traversal_vertex(store, *logical, Some(direction)).await,
                Err(PlanQueryError::UnsupportedOp(_))
            )
        {
            let label_id_raw = federated_expand_label_id_raw(label_id, direction);
            let hits = crate::facade::federation_expand::federated_expand_coordinator(
                store,
                FederatedExpandArgs {
                    logical_vertex_id: *logical,
                    direction: FederatedExpandDirection::Outgoing,
                    label_id_raw,
                },
            )
            .await
            .map_err(|e| PlanQueryError::FederatedIndexCall {
                op: "federated_expand",
                detail: e.to_string(),
            })?;
            out.extend(expand_rows_from_federated_expand_hits(
                store,
                &row,
                &hits,
                dst.as_ref(),
                edge.as_ref(),
                emit_edge_binding,
                dst_property_projection,
                dst_filter,
                parameters,
                caller,
                gleaph_weight_decoders,
                &evaluator,
            )?);
            continue;
        }

        if matches!(direction, EdgeDirection::Undirected)
            && let Some(PlanBinding::RemoteVertex(logical)) = row.get(src.as_ref())
            && matches!(
                resolve_federated_traversal_vertex(store, *logical, Some(direction)).await,
                Err(PlanQueryError::UnsupportedOp(_))
            )
        {
            let label_id_raw = federated_expand_label_id_raw(label_id, direction);
            let hits = crate::facade::federation_expand::federated_expand_coordinator(
                store,
                FederatedExpandArgs {
                    logical_vertex_id: *logical,
                    direction: FederatedExpandDirection::Undirected,
                    label_id_raw,
                },
            )
            .await
            .map_err(|e| PlanQueryError::FederatedIndexCall {
                op: "federated_expand",
                detail: e.to_string(),
            })?;
            out.extend(expand_rows_from_federated_expand_hits(
                store,
                &row,
                &hits,
                dst.as_ref(),
                edge.as_ref(),
                emit_edge_binding,
                dst_property_projection,
                dst_filter,
                parameters,
                caller,
                gleaph_weight_decoders,
                &evaluator,
            )?);
            continue;
        }

        let Some(src_id) = (match row.get(src.as_ref()) {
            Some(PlanBinding::RemoteVertex(logical)) => {
                resolve_federated_traversal_vertex(store, *logical, Some(direction)).await?
            }
            _ => vertex_binding_for_traversal(store, &row, src, Some(direction)).await?,
        }) else {
            continue;
        };
        if let Some(fast_path) = csr_expand_fast_path {
            let mut offset_slot = 0;
            let mut visit = |edge: Edge| {
                let Some(edge_dst) = ExpandDst::from_edge(store, &edge)? else {
                    return Ok(false);
                };
                let edge_binding = edge_binding_for_expand(store, src_id, direction, edge)?;
                if !edge_matches_stream_filter(
                    store,
                    edge_equality_filter
                        .as_ref()
                        .expect("filter exists with fast path"),
                    direction,
                    edge_binding.handle.owner_vertex_id,
                    LaraLabelId::from_raw(edge.label_id),
                    edge.edge_slot_index,
                )? {
                    return Ok(false);
                }
                if !expand_dst_matches_prebound_vertex(&row, dst, edge_dst) {
                    return Ok(false);
                }
                if let ExpandDst::Local(dst_id) = edge_dst {
                    if dst_only_prefilter
                        && !vertex_row_matches_dst_filters(
                            store,
                            parameters,
                            dst,
                            dst_id,
                            dst_filter,
                            caller,
                            gleaph_weight_decoders,
                        )?
                    {
                        return Ok(false);
                    }
                } else if !expand_accepts_remote_dst(dst_only_prefilter, dst_property_projection) {
                    return Ok(false);
                }
                let expanded = build_expanded_row(
                    None,
                    store,
                    &row,
                    edge_key.as_deref(),
                    dst_key.as_str(),
                    edge_dst,
                    edge_binding,
                    edge_property_projection,
                    dst_property_projection,
                )?;
                if !dst_only_prefilter && !row_matches_all(&evaluator, &expanded, dst_filter)? {
                    return Ok(false);
                }
                out.push(expanded);
                Ok(false)
            };
            let res =
                visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
            match res {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(e.into()),
            }
            continue;
        }
        if let Some((edge_label_id, predicate)) = prepared_vector_dst_only_predicate.as_ref() {
            expand_vector_dst_only_rows_into(
                store,
                &row,
                src_id,
                direction,
                *edge_label_id,
                sequence_order,
                dst,
                dst_key.as_str(),
                dst_filter,
                dst_only_prefilter,
                dst_property_projection,
                parameters,
                caller,
                gleaph_weight_decoders,
                &evaluator,
                predicate,
                &mut out,
                &mut vector_batch_scratch,
                &mut vector_matches,
            )?;
            continue;
        }
        candidates.clear();
        expand_candidates_into(
            store,
            src_id,
            direction,
            label_id,
            sequence_order,
            indexed_edge_equality,
            edge_value_predicate,
            edge_vector_predicate,
            parameters,
            &mut candidates,
        )?;
        for (edge_dst, edge_binding) in candidates.iter().copied() {
            if !expand_dst_matches_prebound_vertex(&row, dst, edge_dst) {
                continue;
            }
            if let ExpandDst::Local(dst_id) = edge_dst {
                if dst_only_prefilter
                    && !vertex_row_matches_dst_filters(
                        store,
                        parameters,
                        dst,
                        dst_id,
                        dst_filter,
                        caller,
                        gleaph_weight_decoders,
                    )?
                {
                    continue;
                }
            } else if !expand_accepts_remote_dst(dst_only_prefilter, dst_property_projection) {
                continue;
            }
            let expanded = build_expanded_row(
                None,
                store,
                &row,
                edge_key.as_deref(),
                dst_key.as_str(),
                edge_dst,
                edge_binding,
                edge_property_projection,
                dst_property_projection,
            )?;
            if !dst_only_prefilter && !row_matches_all(&evaluator, &expanded, dst_filter)? {
                continue;
            }
            out.push(expanded);
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn prepare_vector_dst_only_expand_predicate(
    store: &GraphStore,
    label_id: Option<EdgeLabelId>,
    direction: EdgeDirection,
    emit_edge_binding: bool,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_value_predicate: Option<&EdgeValuePredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    edge_property_projection: Option<&[Str]>,
    parameters: &BTreeMap<String, Value>,
) -> Result<Option<(EdgeLabelId, PreparedEdgeVectorThreshold)>, PlanQueryError> {
    if emit_edge_binding
        || indexed_edge_equality.is_some()
        || edge_value_predicate.is_some()
        || edge_vector_predicate.is_none()
        || edge_property_projection.is_some_and(|props| !props.is_empty())
        || !matches!(
            direction,
            EdgeDirection::PointingRight | EdgeDirection::PointingLeft
        )
    {
        return Ok(None);
    }
    let Some(edge_label_id) = label_id else {
        return Ok(None);
    };
    let Some(predicate) = PreparedEdgeVectorThreshold::prepare(
        store,
        edge_label_id,
        edge_vector_predicate.expect("checked above"),
        parameters,
    )?
    else {
        return Ok(None);
    };
    Ok(Some((edge_label_id, predicate)))
}

pub(crate) fn expand_dst_matches_prebound_vertex(
    row: &PlanRow,
    dst: &Str,
    edge_dst: ExpandDst,
) -> bool {
    match (row.get(dst.as_ref()), edge_dst) {
        (Some(PlanBinding::Vertex(id)), ExpandDst::Local(dst_id)) => *id == dst_id,
        (Some(PlanBinding::RemoteVertex(logical)), ExpandDst::Remote(dst_logical)) => {
            *logical == dst_logical
        }
        (None, _) => true,
        _ => false,
    }
}

pub(crate) type ExpandCandidate = (ExpandDst, EdgeBinding);


pub(crate) fn expand_candidates_matching_edge_value_into(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: EdgeLabelId,
    sequence_order: EdgeSequenceOrder,
    predicate: &PreparedEdgeValuePredicate,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let storage_label = LaraLabelId::from_raw(edge_label_id.pack(EdgeDirectedness::Directed).raw());
    let order = sequence_order.into();
    let mut scratch = LabeledEdgeValueBatchScratch::default();
    let mut matches = Vec::new();
    let mut error = None;
    let mut visit_batch = |batch: ic_stable_lara::labeled::LabeledEdgeValueBatch<'_, Edge>| {
        if error.is_some() {
            return;
        }
        matches.clear();
        predicate.kernel.collect_matching_value_indices(
            batch.value_bytes,
            predicate.op,
            &predicate.expected,
            &mut matches,
        );
        let width = usize::from(batch.width_code.byte_width());
        for idx in matches.iter().copied() {
            let Some(edge) = batch.edges.get(idx).copied() else {
                continue;
            };
            let value_start = idx * width;
            let value_end = value_start + width;
            let edge = edge.with_value_bytes(&batch.value_bytes[value_start..value_end]);
            match ExpandDst::from_edge(store, &edge).and_then(|edge_dst| match edge_dst {
                Some(edge_dst) => push_scanned_value_expand_candidate(
                    out, store, src_id, direction, edge_dst, edge,
                ),
                None => Ok(()),
            }) {
                Ok(()) => {}
                Err(err) => {
                    error = Some(err);
                    return;
                }
            }
        }
    };

    match direction {
        EdgeDirection::PointingRight => store
            .visit_out_edge_value_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_edge_value_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    if let Some(err) = error {
        return Err(err);
    }
    Ok(())
}

pub(crate) fn expand_candidates_matching_edge_vector_threshold_into(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: EdgeLabelId,
    sequence_order: EdgeSequenceOrder,
    predicate: &PreparedEdgeVectorThreshold,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let storage_label = LaraLabelId::from_raw(edge_label_id.pack(EdgeDirectedness::Directed).raw());
    let order = sequence_order.into();
    let mut scratch = LabeledEdgeValueBatchScratch::default();
    let mut matches = Vec::new();
    let mut error = None;
    let mut visit_batch = |batch: ic_stable_lara::labeled::LabeledEdgeValueBatch<'_, Edge>| {
        if error.is_some() {
            return;
        }
        matches.clear();
        predicate.collect_matching_indices(batch.value_bytes, &mut matches);
        if !matches.is_empty() {
            out.reserve(matches.len());
        }
        let width = predicate.kernel.byte_width();
        for idx in matches.iter().copied() {
            let Some(edge) = batch.edges.get(idx).copied() else {
                continue;
            };
            let value_start = idx * width;
            let value_end = value_start + width;
            let edge = edge.with_value_bytes(&batch.value_bytes[value_start..value_end]);
            match ExpandDst::from_edge(store, &edge).and_then(|edge_dst| match edge_dst {
                Some(edge_dst) => push_scanned_value_expand_candidate(
                    out, store, src_id, direction, edge_dst, edge,
                ),
                None => Ok(()),
            }) {
                Ok(()) => {}
                Err(err) => {
                    error = Some(err);
                    return;
                }
            }
        }
    };

    match direction {
        EdgeDirection::PointingRight => store
            .visit_out_edge_value_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_edge_value_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    if let Some(err) = error {
        return Err(err);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn expand_vector_dst_only_rows_into(
    store: &GraphStore,
    row: &PlanRow,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: EdgeLabelId,
    sequence_order: EdgeSequenceOrder,
    dst: &Str,
    dst_key: &str,
    dst_filter: &[Expr],
    dst_only_prefilter: bool,
    dst_property_projection: Option<&[Str]>,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    predicate: &PreparedEdgeVectorThreshold,
    out: &mut Vec<PlanRow>,
    scratch: &mut LabeledEdgeValueBatchScratch<Edge>,
    matches: &mut Vec<usize>,
) -> Result<(), PlanQueryError> {
    let storage_label = LaraLabelId::from_raw(edge_label_id.pack(EdgeDirectedness::Directed).raw());
    let order = sequence_order.into();
    let mut error = None;
    let mut visit_batch = |batch: ic_stable_lara::labeled::LabeledEdgeValueBatch<'_, Edge>| {
        if error.is_some() {
            return;
        }
        matches.clear();
        predicate.collect_matching_indices(batch.value_bytes, matches);
        if !matches.is_empty() {
            out.reserve(matches.len());
        }
        for idx in matches.iter().copied() {
            let Some(edge) = batch.edges.get(idx).copied() else {
                continue;
            };
            match ExpandDst::from_edge(store, &edge).and_then(|edge_dst| {
                let Some(edge_dst) = edge_dst else {
                    return Ok(());
                };
                if !expand_dst_matches_prebound_vertex(row, dst, edge_dst) {
                    return Ok(());
                }
                if let ExpandDst::Local(dst_id) = edge_dst {
                    if dst_only_prefilter
                        && !vertex_row_matches_dst_filters(
                            store,
                            parameters,
                            dst,
                            dst_id,
                            dst_filter,
                            caller,
                            gleaph_weight_decoders,
                        )?
                    {
                        return Ok(());
                    }
                } else if !expand_accepts_remote_dst(dst_only_prefilter, dst_property_projection) {
                    return Ok(());
                }
                let dst_binding = expand_dst_binding(store, edge_dst, dst_property_projection)?;
                let expanded = row.fork([(dst_key, dst_binding)]);
                if !dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
                    return Ok(());
                }
                out.push(expanded);
                Ok(())
            }) {
                Ok(()) => {}
                Err(err) => {
                    error = Some(err);
                    return;
                }
            }
        }
    };

    match direction {
        EdgeDirection::PointingRight => store
            .visit_out_edge_value_batches_for_label(
                src_id,
                storage_label,
                order,
                scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_edge_value_batches_for_label(
                src_id,
                storage_label,
                order,
                scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    if let Some(err) = error {
        return Err(err);
    }
    Ok(())
}

pub(crate) fn expand_candidates_into(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: Option<EdgeLabelId>,
    sequence_order: EdgeSequenceOrder,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_value_predicate: Option<&EdgeValuePredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    parameters: &BTreeMap<String, Value>,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let indexed = indexed_edge_equality.map(|(property, value)| (property.as_ref(), value));
    if let Some((property, scan_value)) = indexed
        && expand_candidates_via_equality_index(
            store,
            src_id,
            direction,
            edge_label_id,
            property,
            scan_value,
            parameters,
            out,
        )?
    {
        return Ok(());
    }
    if let Some(edge_value_predicate) = edge_value_predicate {
        let Some(edge_label_id) = edge_label_id else {
            return Ok(());
        };
        let Some(predicate) = PreparedEdgeValuePredicate::prepare(
            store,
            edge_label_id,
            edge_value_predicate,
            parameters,
        )?
        else {
            return Ok(());
        };
        expand_candidates_matching_edge_value_into(
            store,
            src_id,
            direction,
            edge_label_id,
            sequence_order,
            &predicate,
            out,
        )?;
        return Ok(());
    }
    if let Some(edge_vector_predicate) = edge_vector_predicate {
        let Some(edge_label_id) = edge_label_id else {
            return Ok(());
        };
        let Some(predicate) = PreparedEdgeVectorThreshold::prepare(
            store,
            edge_label_id,
            edge_vector_predicate,
            parameters,
        )?
        else {
            return Ok(());
        };
        expand_candidates_matching_edge_vector_threshold_into(
            store,
            src_id,
            direction,
            edge_label_id,
            sequence_order,
            &predicate,
            out,
        )?;
        return Ok(());
    }

    match direction {
        EdgeDirection::PointingRight => {
            let mut error = None;
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                sequence_order,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    if let Some((property, scan_value)) = indexed {
                        match edge_matches_indexed_equality(
                            store,
                            src_id,
                            direction,
                            LaraLabelId::from_raw(edge.label_id),
                            edge.edge_slot_index,
                            &edge,
                            property,
                            scan_value,
                            parameters,
                        ) {
                            Ok(false) => return,
                            Ok(true) => {}
                            Err(err) => {
                                error = Some(err);
                                return;
                            }
                        }
                    }
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(store, &edge)
                        && let Err(err) =
                            push_expand_candidate(out, store, src_id, direction, edge_dst, edge)
                    {
                        error = Some(err);
                    }
                },
            )?;
            if let Some(err) = error {
                return Err(err);
            }
        }
        EdgeDirection::PointingLeft => {
            let mut error = None;
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                sequence_order,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    if let Some((property, scan_value)) = indexed {
                        match edge_matches_indexed_equality(
                            store,
                            src_id,
                            direction,
                            LaraLabelId::from_raw(edge.label_id),
                            edge.edge_slot_index,
                            &edge,
                            property,
                            scan_value,
                            parameters,
                        ) {
                            Ok(false) => return,
                            Ok(true) => {}
                            Err(err) => {
                                error = Some(err);
                                return;
                            }
                        }
                    }
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(store, &edge)
                        && let Err(err) =
                            push_expand_candidate(out, store, src_id, direction, edge_dst, edge)
                    {
                        error = Some(err);
                    }
                },
            )?;
            if let Some(err) = error {
                return Err(err);
            }
        }
        EdgeDirection::Undirected => {
            let mut error = None;
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                sequence_order,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    if let Some((property, scan_value)) = indexed {
                        match edge_matches_indexed_equality(
                            store,
                            src_id,
                            direction,
                            LaraLabelId::from_raw(edge.label_id),
                            edge.edge_slot_index,
                            &edge,
                            property,
                            scan_value,
                            parameters,
                        ) {
                            Ok(false) => return,
                            Ok(true) => {}
                            Err(err) => {
                                error = Some(err);
                                return;
                            }
                        }
                    }
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(store, &edge)
                        && let Err(err) =
                            push_expand_candidate(out, store, src_id, direction, edge_dst, edge)
                    {
                        error = Some(err);
                    }
                },
            )?;
            if let Some(err) = error {
                return Err(err);
            }
        }
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    Ok(())
}

fn for_each_csr_expand_edge<F>(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: Option<EdgeLabelId>,
    sequence_order: EdgeSequenceOrder,
    visit: F,
) -> Result<(), PlanQueryError>
where
    F: FnMut(Edge),
{
    let order = sequence_order.into();
    match direction {
        EdgeDirection::PointingRight => {
            if let Some(lid) = edge_label_id {
                store
                    .for_each_directed_out_edges_for_label_with_values(src_id, lid, order, visit)?;
            } else {
                store.for_each_directed_out_edges(src_id, order, visit)?;
            }
            Ok(())
        }
        EdgeDirection::Undirected => {
            if let Some(lid) = edge_label_id {
                store.for_each_undirected_edges_for_label(src_id, lid, order, visit)?;
            } else {
                store.for_each_undirected_edges(src_id, order, visit)?;
            }
            Ok(())
        }
        EdgeDirection::PointingLeft => {
            if let Some(lid) = edge_label_id {
                store
                    .for_each_directed_in_edges_for_label_with_values(src_id, lid, order, visit)?;
            } else {
                store.for_each_directed_in_edges(src_id, order, visit)?;
            }
            Ok(())
        }
        other => Err(PlanQueryError::UnsupportedDirection(other)),
    }
}

/// Probes the in-process edge equality index and, on hit, enumerates only matching slots.
/// Returns `Ok(true)` when the index owned the lookup (including zero matches).
fn expand_candidates_via_equality_index(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: Option<EdgeLabelId>,
    property: &str,
    scan_value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
    out: &mut Vec<ExpandCandidate>,
) -> Result<bool, PlanQueryError> {
    let Some(property_id) = store.property_id(property) else {
        return Ok(false);
    };
    let Some(expected) = resolve_scan_value_bytes(scan_value, parameters)? else {
        out.clear();
        return Ok(true);
    };
    let Some(postings) = edge_equal::lookup_equal(property_id, &expected) else {
        return Ok(false);
    };

    let mut out_slots: BTreeSet<(u16, u32)> = BTreeSet::new();
    let mut in_slots: BTreeSet<(u32, u16, u32)> = BTreeSet::new();
    for posting in &postings {
        if posting.owner_vertex_id == src_id {
            out_slots.insert((posting.label_id, posting.slot_index));
        }
        in_slots.insert((
            u32::from(posting.owner_vertex_id),
            posting.label_id,
            posting.slot_index,
        ));
    }

    let mut error = None;
    match direction {
        EdgeDirection::PointingRight => {
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                EdgeSequenceOrder::Descending,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    if !out_slots.contains(&(edge.label_id, edge.edge_slot_index.raw())) {
                        return;
                    }
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(store, &edge)
                        && let Err(err) =
                            push_expand_candidate(out, store, src_id, direction, edge_dst, edge)
                    {
                        error = Some(err);
                    }
                },
            )?;
        }
        EdgeDirection::PointingLeft => {
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                EdgeSequenceOrder::Descending,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    let canonical = store.canonical_edge_handle(EdgeHandle {
                        owner_vertex_id: src_id,
                        label_id: LaraLabelId::from_raw(edge.label_id),
                        slot_index: edge.edge_slot_index.raw(),
                    });
                    if !in_slots.contains(&(
                        u32::from(canonical.owner_vertex_id),
                        canonical.label_id.raw(),
                        canonical.slot_index,
                    )) {
                        return;
                    }
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(store, &edge)
                        && let Err(err) =
                            push_expand_candidate(out, store, src_id, direction, edge_dst, edge)
                    {
                        error = Some(err);
                    }
                },
            )?;
        }
        EdgeDirection::Undirected => {
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                EdgeSequenceOrder::Descending,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    let canonical = store.canonical_edge_handle(EdgeHandle {
                        owner_vertex_id: src_id,
                        label_id: LaraLabelId::from_raw(edge.label_id),
                        slot_index: edge.edge_slot_index.raw(),
                    });
                    if !in_slots.contains(&(
                        u32::from(canonical.owner_vertex_id),
                        canonical.label_id.raw(),
                        canonical.slot_index,
                    )) {
                        return;
                    }
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(store, &edge)
                        && let Err(err) =
                            push_expand_candidate(out, store, src_id, direction, edge_dst, edge)
                    {
                        error = Some(err);
                    }
                },
            )?;
        }
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    if let Some(err) = error {
        return Err(err);
    }
    Ok(true)
}

mod predicates;
pub(crate) use predicates::{PreparedEdgeValuePredicate, PreparedEdgeVectorThreshold};

#[cfg(test)]
mod tests;
