use std::collections::{BTreeMap, BTreeSet};

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::plan::{EdgePayloadPredicate, EdgeVectorPredicate, ScanValue, Str};
use gleaph_graph_kernel::entry::{Edge, EdgeDirectedness, EdgeLabelId, PreparedWeightDecoder};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::{
    LabeledEdgePayloadBatchScratch, LabeledPayloadValueBatchScratch, OutEdgeOrder,
};

use super::label_expr::fusion_edge_label_ids_for_expr;
use super::predicates::{PreparedEdgePayloadPredicate, PreparedEdgeVectorThreshold};
use super::{
    ExpandDst, edge_matches_indexed_equality, expand_accepts_remote_dst, expand_dst_binding,
    expand_dst_matches_prebound_vertex, push_expand_candidate, push_scanned_value_expand_candidate,
    row_matches_all,
};
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::edge_equal;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::bindings::EdgeBinding;
use crate::plan::query::executor::context::QueryExprEvaluator;
use crate::plan::query::executor::{
    EdgeSequenceOrder, resolve_scan_payload_bytes, vertex_row_matches_dst_filters,
};
use crate::plan::query::row::PlanRow;
pub(crate) type ExpandCandidate = (ExpandDst, EdgeBinding);

pub(crate) fn expand_candidates_matching_edge_payload_into(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: EdgeLabelId,
    sequence_order: EdgeSequenceOrder,
    predicate: &PreparedEdgePayloadPredicate,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let storage_label = LaraLabelId::from_raw(edge_label_id.pack(EdgeDirectedness::Directed).raw());
    let order = sequence_order.into();
    if try_expand_matching_edge_payload_payload_first(
        store,
        src_id,
        direction,
        storage_label,
        order,
        predicate,
        out,
    )? {
        return Ok(());
    }
    expand_matching_edge_payload_combined_batch(
        store,
        src_id,
        direction,
        storage_label,
        order,
        predicate,
        out,
    )
}

fn try_expand_matching_edge_payload_payload_first(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    storage_label: LaraLabelId,
    order: OutEdgeOrder,
    predicate: &PreparedEdgePayloadPredicate,
    out: &mut Vec<ExpandCandidate>,
) -> Result<bool, PlanQueryError> {
    let mut saw_dense = false;
    let mut pending = Vec::new();
    let mut value_scratch = LabeledPayloadValueBatchScratch::default();
    let mut visit_values = |batch: ic_stable_lara::labeled::LabeledPayloadValueBatch<'_>| {
        if batch.dense {
            saw_dense = true;
        }
        let mut matches = Vec::new();
        predicate.kernel.collect_matching_value_indices(
            batch.values,
            predicate.op,
            &predicate.expected,
            &mut matches,
        );
        let width = usize::from(batch.byte_width);
        for idx in matches {
            let Some(&slot) = batch.slot_indices.get(idx) else {
                continue;
            };
            let payload_start = idx * width;
            let payload_end = payload_start + width;
            pending.push((slot, batch.values[payload_start..payload_end].to_vec()));
        }
    };

    match direction {
        EdgeDirection::PointingRight => store
            .visit_out_payload_value_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut value_scratch,
                &mut visit_values,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_payload_value_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut value_scratch,
                &mut visit_values,
            )
            .map_err(GraphStoreError::from)?,
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }

    if !saw_dense {
        return Ok(false);
    }

    if pending.is_empty() {
        return Ok(true);
    }

    let payload_by_slot: BTreeMap<u32, Vec<u8>> = pending.into_iter().collect();
    let slots: Vec<u32> = payload_by_slot.keys().copied().collect();
    let mut error = None;
    let mut visit_edge = |edge: Edge| {
        if error.is_some() {
            return;
        }
        let Some(payload) = payload_by_slot.get(&edge.edge_slot_index.raw()) else {
            return;
        };
        let edge = edge.with_payload_bytes(payload);
        match ExpandDst::from_edge(store, &edge).and_then(|edge_dst| match edge_dst {
            Some(edge_dst) => {
                push_scanned_value_expand_candidate(out, store, src_id, direction, edge_dst, edge)
            }
            None => Ok(()),
        }) {
            Ok(()) => {}
            Err(err) => error = Some(err),
        }
    };

    match direction {
        EdgeDirection::PointingRight => store
            .read_out_edge_slots_for_label(src_id, storage_label, &slots, order, &mut visit_edge)
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .read_in_edge_slots_for_label(src_id, storage_label, &slots, order, &mut visit_edge)
            .map_err(GraphStoreError::from)?,
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    if let Some(err) = error {
        return Err(err);
    }
    Ok(true)
}

fn expand_matching_edge_payload_combined_batch(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    storage_label: LaraLabelId,
    order: OutEdgeOrder,
    predicate: &PreparedEdgePayloadPredicate,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let mut scratch = LabeledEdgePayloadBatchScratch::default();
    let mut matches = Vec::new();
    let mut error = None;
    let mut visit_batch = |batch: ic_stable_lara::labeled::LabeledEdgePayloadBatch<'_, Edge>| {
        if error.is_some() {
            return;
        }
        matches.clear();
        predicate.kernel.collect_matching_value_indices(
            batch.payload_bytes,
            predicate.op,
            &predicate.expected,
            &mut matches,
        );
        let width = usize::from(batch.byte_width);
        for idx in matches.iter().cloned() {
            let Some(edge) = batch.edges.get(idx).cloned() else {
                continue;
            };
            let payload_start = idx * width;
            let payload_end = payload_start + width;
            let edge = edge.with_payload_bytes(&batch.payload_bytes[payload_start..payload_end]);
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
            .visit_out_edge_payload_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_edge_payload_batches_for_label(
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
    let mut scratch = LabeledEdgePayloadBatchScratch::default();
    let mut matches = Vec::new();
    let mut error = None;
    let mut visit_batch = |batch: ic_stable_lara::labeled::LabeledEdgePayloadBatch<'_, Edge>| {
        if error.is_some() {
            return;
        }
        matches.clear();
        predicate.collect_matching_indices(batch.payload_bytes, &mut matches);
        if !matches.is_empty() {
            out.reserve(matches.len());
        }
        let width = predicate.kernel.byte_width();
        for idx in matches.iter().cloned() {
            let Some(edge) = batch.edges.get(idx).cloned() else {
                continue;
            };
            let payload_start = idx * width;
            let payload_end = payload_start + width;
            let edge = edge.with_payload_bytes(&batch.payload_bytes[payload_start..payload_end]);
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
            .visit_out_edge_payload_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_edge_payload_batches_for_label(
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
pub(super) fn expand_vector_dst_only_rows_into(
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
    scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
    matches: &mut Vec<usize>,
) -> Result<(), PlanQueryError> {
    let storage_label = LaraLabelId::from_raw(edge_label_id.pack(EdgeDirectedness::Directed).raw());
    let order = sequence_order.into();
    let mut error = None;
    let mut visit_batch = |batch: ic_stable_lara::labeled::LabeledEdgePayloadBatch<'_, Edge>| {
        if error.is_some() {
            return;
        }
        matches.clear();
        predicate.collect_matching_indices(batch.payload_bytes, matches);
        if !matches.is_empty() {
            out.reserve(matches.len());
        }
        for idx in matches.iter().cloned() {
            let Some(edge) = batch.edges.get(idx).cloned() else {
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
            .visit_out_edge_payload_batches_for_label(
                src_id,
                storage_label,
                order,
                scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_edge_payload_batches_for_label(
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

/// Expand with index/payload/vector predicates decomposed across labels in `label_expr`.
///
/// Returns `true` when fusion ran (including zero matches). Returns `false` when the caller
/// should use the generic `expand_candidates_into` path.
pub(crate) fn expand_candidates_with_label_expr_fusion_into(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    src_id: VertexId,
    direction: EdgeDirection,
    label_expr: &LabelExpr,
    sequence_order: EdgeSequenceOrder,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_payload_predicate: Option<&EdgePayloadPredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    parameters: &BTreeMap<String, Value>,
    out: &mut Vec<ExpandCandidate>,
) -> Result<bool, PlanQueryError> {
    let has_predicate = indexed_edge_equality.is_some()
        || edge_payload_predicate.is_some()
        || edge_vector_predicate.is_some();
    if !has_predicate {
        return Ok(false);
    }
    let Some(label_ids) = fusion_edge_label_ids_for_expr(execution, label_expr) else {
        return Ok(false);
    };
    for label_id in label_ids {
        expand_candidates_into(
            store,
            src_id,
            direction,
            Some(label_id),
            sequence_order,
            indexed_edge_equality,
            edge_payload_predicate,
            edge_vector_predicate,
            parameters,
            out,
        )?;
    }
    Ok(true)
}

pub(crate) fn expand_candidates_for_expand_op_into(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: Option<EdgeLabelId>,
    label_expr: Option<&LabelExpr>,
    sequence_order: EdgeSequenceOrder,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_payload_predicate: Option<&EdgePayloadPredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    parameters: &BTreeMap<String, Value>,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    if edge_label_id.is_none()
        && let Some(expr) = label_expr
        && expand_candidates_with_label_expr_fusion_into(
            store,
            execution,
            src_id,
            direction,
            expr,
            sequence_order,
            indexed_edge_equality,
            edge_payload_predicate,
            edge_vector_predicate,
            parameters,
            out,
        )?
    {
        return Ok(());
    }
    expand_candidates_into(
        store,
        src_id,
        direction,
        edge_label_id,
        sequence_order,
        indexed_edge_equality,
        edge_payload_predicate,
        edge_vector_predicate,
        parameters,
        out,
    )
}

pub(crate) fn expand_candidates_into(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: Option<EdgeLabelId>,
    sequence_order: EdgeSequenceOrder,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_payload_predicate: Option<&EdgePayloadPredicate>,
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
    if let Some(edge_payload_predicate) = edge_payload_predicate {
        let Some(edge_label_id) = edge_label_id else {
            return Ok(());
        };
        let Some(predicate) = PreparedEdgePayloadPredicate::prepare(
            store,
            edge_label_id,
            edge_payload_predicate,
            parameters,
        )?
        else {
            return Ok(());
        };
        expand_candidates_matching_edge_payload_into(
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
                store.for_each_directed_out_edges_for_label_with_payloads(
                    src_id, lid, order, visit,
                )?;
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
                store.for_each_directed_in_edges_for_label_with_payloads(
                    src_id, lid, order, visit,
                )?;
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
    let Some(expected) = resolve_scan_payload_bytes(scan_value, parameters)? else {
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

    let order = OutEdgeOrder::Descending;
    let mut error = None;
    match direction {
        EdgeDirection::PointingRight => {
            let wire_filter =
                edge_label_id.map(|label_id| label_id.pack(EdgeDirectedness::Directed).raw());
            let mut slots_by_label: BTreeMap<u16, Vec<u32>> = BTreeMap::new();
            for (label_id, slot_index) in &out_slots {
                if wire_filter.is_some_and(|wire| wire != *label_id) {
                    continue;
                }
                slots_by_label
                    .entry(*label_id)
                    .or_default()
                    .push(*slot_index);
            }
            for (label_id, slots) in slots_by_label {
                let storage_label = LaraLabelId::from_raw(label_id);
                store
                    .read_out_edge_slots_for_label(src_id, storage_label, &slots, order, |edge| {
                        if error.is_some() {
                            return;
                        }
                        if let Ok(Some(edge_dst)) = ExpandDst::from_edge(store, &edge)
                            && let Err(err) =
                                push_expand_candidate(out, store, src_id, direction, edge_dst, edge)
                        {
                            error = Some(err);
                        }
                    })
                    .map_err(GraphStoreError::from)?;
            }
        }
        // Postings record forward-owner slots; reverse/undirected expand still needs a full
        // adjacency scan plus canonical handle matching to locate incoming edges at `src_id`.
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
