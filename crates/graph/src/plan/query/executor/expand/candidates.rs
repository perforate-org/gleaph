use std::collections::{BTreeMap, BTreeSet};

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::plan::{
    EdgeInlinePropertyPredicate, EdgeInlineVectorPredicate, ScanValue, Str,
};
use gleaph_graph_kernel::entry::{Edge, EdgeDirectedness, EdgeLabelId};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::{
    LabeledEdgeInlinePropertyBatchScratch, LabeledInlinePropertyValueBatchScratch, OutEdgeOrder,
};
use ic_stable_lara::traits::CsrEdge;
use ic_stable_lara::traverse::BucketEntryPosition;

use super::label_expr::{
    catalog_edge_label_ids_for_predicate_fusion, fusion_edge_label_ids_for_expr,
};
use super::predicates::{PreparedEdgeInlinePropertyPredicate, PreparedEdgeInlineVectorThreshold};
use super::{
    ExpandDst, edge_matches_indexed_equality, expand_accepts_remote_dst, expand_dst_binding,
    expand_dst_matches_prebound_vertex, push_expand_candidate, push_scanned_value_expand_candidate,
    row_matches_all,
};
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::edge_lookup;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::bindings::EdgeBinding;
use crate::plan::query::executor::context::QueryExprEvaluator;
use crate::plan::query::executor::{
    EdgeSequenceOrder, resolve_edge_equality_probe_payloads, vertex_row_matches_dst_filters,
};
use crate::plan::query::row::PlanRow;
pub(crate) type ExpandCandidate = (ExpandDst, EdgeBinding);

pub(crate) fn expand_candidates_matching_edge_inline_property_into(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: EdgeLabelId,
    sequence_order: EdgeSequenceOrder,
    predicate: &PreparedEdgeInlinePropertyPredicate,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let storage_label = LaraLabelId::from_raw(edge_label_id.pack(EdgeDirectedness::Directed).raw());
    let order = sequence_order.into();
    if try_expand_matching_edge_inline_property_inline_property_bytes_first(
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
    expand_matching_edge_inline_property_combined_batch(
        store,
        src_id,
        direction,
        storage_label,
        order,
        predicate,
        out,
    )
}

fn try_expand_matching_edge_inline_property_inline_property_bytes_first(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    storage_label: LaraLabelId,
    order: OutEdgeOrder,
    predicate: &PreparedEdgeInlinePropertyPredicate,
    out: &mut Vec<ExpandCandidate>,
) -> Result<bool, PlanQueryError> {
    let inline_property_bytes_first_eligible = match direction {
        EdgeDirection::PointingRight => store
            .out_label_bucket_inline_property_bytes_first_predicate_eligible(
                src_id,
                storage_label,
            )?,
        EdgeDirection::PointingLeft => store
            .in_label_bucket_inline_property_bytes_first_predicate_eligible(
                src_id,
                storage_label,
            )?,
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    };
    if !inline_property_bytes_first_eligible {
        return Ok(false);
    }

    let mut pending_slots = Vec::<BucketEntryPosition>::new();
    let mut pending_inline_property_bytes = Vec::new();
    let mut value_scratch = LabeledInlinePropertyValueBatchScratch::default();
    let mut visit_values = |batch: ic_stable_lara::labeled::LabeledInlinePropertyValueBatch<'_>| {
        let mut matches = Vec::new();
        predicate.kernel.collect_matching_value_indices(
            batch.values,
            predicate.op,
            &predicate.expected,
            &mut matches,
        );
        let batch_width = usize::from(batch.byte_width);
        for idx in matches {
            let Some(&slot) = batch.slot_indices.get(idx) else {
                continue;
            };
            let inline_property_bytes_start = idx * batch_width;
            let inline_property_bytes_end = inline_property_bytes_start + batch_width;
            pending_slots.push(BucketEntryPosition::new(slot));
            pending_inline_property_bytes.extend_from_slice(
                &batch.values[inline_property_bytes_start..inline_property_bytes_end],
            );
        }
    };

    match direction {
        EdgeDirection::PointingRight => store
            .visit_out_inline_property_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut value_scratch,
                &mut visit_values,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_inline_property_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut value_scratch,
                &mut visit_values,
            )
            .map_err(GraphStoreError::from)?,
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }

    if pending_slots.is_empty() {
        value_scratch.clear();
        return Ok(true);
    }

    let width = usize::from(predicate.kernel.byte_width());
    let inline_property_bytes_by_slot: BTreeMap<u32, &[u8]> = pending_slots
        .iter()
        .enumerate()
        .map(|(idx, &slot)| {
            let start = idx * width;
            (
                slot.raw(),
                &pending_inline_property_bytes[start..start + width],
            )
        })
        .collect();
    let mut error = None;
    let mut visit_edge = |edge: Edge| {
        if error.is_some() {
            return;
        }
        let Some(inline_property_bytes) =
            inline_property_bytes_by_slot.get(&edge.edge_slot_index.raw())
        else {
            return;
        };
        let edge = edge.with_stored_inline_property_bytes(width as u16, inline_property_bytes);
        match ExpandDst::from_edge(&edge).and_then(|edge_dst| match edge_dst {
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
            .read_out_edge_slots_for_label(
                src_id,
                storage_label,
                &pending_slots,
                order,
                &mut visit_edge,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .read_in_edge_slots_for_label(
                src_id,
                storage_label,
                &pending_slots,
                order,
                &mut visit_edge,
            )
            .map_err(GraphStoreError::from)?,
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    value_scratch.clear();
    if let Some(err) = error {
        return Err(err);
    }
    Ok(true)
}

fn expand_matching_edge_inline_property_combined_batch(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    storage_label: LaraLabelId,
    order: OutEdgeOrder,
    predicate: &PreparedEdgeInlinePropertyPredicate,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let mut scratch = LabeledEdgeInlinePropertyBatchScratch::default();
    let mut matches = Vec::new();
    let mut error = None;
    let mut visit_batch =
        |batch: ic_stable_lara::labeled::LabeledEdgeInlinePropertyBatch<'_, Edge>| {
            if error.is_some() {
                return;
            }
            matches.clear();
            predicate.kernel.collect_matching_value_indices(
                batch.inline_property_bytes,
                predicate.op,
                &predicate.expected,
                &mut matches,
            );
            let width = usize::from(batch.byte_width);
            for idx in matches.iter().cloned() {
                let Some(edge) = batch.edges.get(idx).cloned() else {
                    continue;
                };
                let inline_property_bytes_start = idx * width;
                let inline_property_bytes_end = inline_property_bytes_start + width;
                let edge = edge.with_stored_inline_property_bytes(
                    width as u16,
                    &batch.inline_property_bytes
                        [inline_property_bytes_start..inline_property_bytes_end],
                );
                match ExpandDst::from_edge(&edge).and_then(|edge_dst| match edge_dst {
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
            .visit_out_edge_inline_property_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_edge_inline_property_batches_for_label(
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
    predicate: &PreparedEdgeInlineVectorThreshold,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let storage_label = LaraLabelId::from_raw(edge_label_id.pack(EdgeDirectedness::Directed).raw());
    let order = sequence_order.into();
    expand_matching_edge_vector_threshold_combined_batch(
        store,
        src_id,
        direction,
        storage_label,
        order,
        predicate,
        out,
    )
}

fn expand_matching_edge_vector_threshold_combined_batch(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    storage_label: LaraLabelId,
    order: OutEdgeOrder,
    predicate: &PreparedEdgeInlineVectorThreshold,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let mut scratch = LabeledEdgeInlinePropertyBatchScratch::default();
    let mut matches = Vec::new();
    let mut error = None;
    let mut visit_batch =
        |batch: ic_stable_lara::labeled::LabeledEdgeInlinePropertyBatch<'_, Edge>| {
            if error.is_some() {
                return;
            }
            matches.clear();
            predicate.collect_matching_indices(batch.inline_property_bytes, &mut matches);
            if !matches.is_empty() {
                out.reserve(matches.len());
            }
            let width = predicate.kernel.byte_width();
            for idx in matches.iter().cloned() {
                let Some(edge) = batch.edges.get(idx).cloned() else {
                    continue;
                };
                let inline_property_bytes_start = idx * width;
                let inline_property_bytes_end = inline_property_bytes_start + width;
                let edge = edge.with_stored_inline_property_bytes(
                    width as u16,
                    &batch.inline_property_bytes
                        [inline_property_bytes_start..inline_property_bytes_end],
                );
                match ExpandDst::from_edge(&edge).and_then(|edge_dst| match edge_dst {
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
            .visit_out_edge_inline_property_batches_for_label(
                src_id,
                storage_label,
                order,
                &mut scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_edge_inline_property_batches_for_label(
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

/// What every "destination-only" vector expansion reads from its caller.
///
/// These eight facts were repeated as arguments by all three functions below, and `store`/`parameters`/`caller` were repeated a
/// second time alongside an evaluator that already holds them - so the duplication was the argument count. The row is the one
/// being expanded, and everything else describes where the destination binding goes and how it is filtered.
pub(super) struct DstOnlyCtx<'a> {
    pub(super) execution: &'a GqlExecutionContext,
    pub(super) row: &'a PlanRow,
    pub(super) dst: &'a Str,
    pub(super) dst_key: &'a str,
    pub(super) dst_filter: &'a [Expr],
    pub(super) dst_only_prefilter: bool,
    pub(super) dst_property_projection: Option<&'a [Str]>,
    pub(super) evaluator: &'a QueryExprEvaluator<'a>,
}

impl DstOnlyCtx<'_> {
    fn store(&self) -> &GraphStore {
        self.evaluator.store
    }

    fn parameters(&self) -> &BTreeMap<String, Value> {
        self.evaluator.parameters
    }

    fn caller(&self) -> Option<Principal> {
        self.evaluator.caller
    }
}

/// Where a batch expansion puts its work: the output rows, the batch scratch, and the match buffer the predicate fills.
pub(super) struct DstOnlySink<'a> {
    pub(super) out: &'a mut Vec<PlanRow>,
    pub(super) scratch: &'a mut LabeledEdgeInlinePropertyBatchScratch<Edge>,
    pub(super) matches: &'a mut Vec<usize>,
}

pub(super) fn expand_vector_dst_only_rows_into(
    ctx: &DstOnlyCtx<'_>,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: EdgeLabelId,
    sequence_order: EdgeSequenceOrder,
    predicate: &PreparedEdgeInlineVectorThreshold,
    sink: &mut DstOnlySink<'_>,
) -> Result<(), PlanQueryError> {
    let storage_label = LaraLabelId::from_raw(edge_label_id.pack(EdgeDirectedness::Directed).raw());
    let order = sequence_order.into();
    expand_vector_dst_only_rows_combined_batch(
        ctx,
        src_id,
        direction,
        storage_label,
        order,
        predicate,
        sink,
    )
}

fn push_vector_dst_only_row_for_edge(
    ctx: &DstOnlyCtx<'_>,
    edge: Edge,
    out: &mut Vec<PlanRow>,
) -> Result<(), PlanQueryError> {
    let DstOnlyCtx {
        execution,
        row,
        dst,
        dst_key,
        dst_filter,
        dst_only_prefilter,
        dst_property_projection,
        evaluator,
    } = ctx;
    let Some(edge_dst) = ExpandDst::from_edge(&edge)? else {
        return Ok(());
    };
    if !expand_dst_matches_prebound_vertex(row, dst, edge_dst) {
        return Ok(());
    }
    if let ExpandDst::Local(dst_id) = edge_dst {
        if *dst_only_prefilter
            && !vertex_row_matches_dst_filters(
                ctx.store(),
                ctx.parameters(),
                dst,
                dst_id,
                dst_filter,
                ctx.caller(),
                execution,
            )?
        {
            return Ok(());
        }
    } else if !expand_accepts_remote_dst(*dst_only_prefilter, *dst_property_projection) {
        return Ok(());
    }
    let dst_binding =
        expand_dst_binding(ctx.store(), execution, edge_dst, *dst_property_projection)?;
    let expanded = row.fork([(*dst_key, dst_binding)]);
    if !*dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
        return Ok(());
    }
    out.push(expanded);
    Ok(())
}

fn expand_vector_dst_only_rows_combined_batch(
    ctx: &DstOnlyCtx<'_>,
    src_id: VertexId,
    direction: EdgeDirection,
    storage_label: LaraLabelId,
    order: OutEdgeOrder,
    predicate: &PreparedEdgeInlineVectorThreshold,
    sink: &mut DstOnlySink<'_>,
) -> Result<(), PlanQueryError> {
    let DstOnlySink {
        out,
        scratch,
        matches,
    } = sink;
    let store = ctx.store();
    let mut error = None;
    let mut visit_batch =
        |batch: ic_stable_lara::labeled::LabeledEdgeInlinePropertyBatch<'_, Edge>| {
            if error.is_some() {
                return;
            }
            matches.clear();
            predicate.collect_matching_indices(batch.inline_property_bytes, matches);
            if !matches.is_empty() {
                out.reserve(matches.len());
            }
            for idx in matches.iter().cloned() {
                let Some(edge) = batch.edges.get(idx).cloned() else {
                    continue;
                };
                match push_vector_dst_only_row_for_edge(ctx, edge, out) {
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
            .visit_out_edge_inline_property_batches_for_label(
                src_id,
                storage_label,
                order,
                scratch,
                &mut visit_batch,
            )
            .map_err(GraphStoreError::from)?,
        EdgeDirection::PointingLeft => store
            .visit_in_edge_inline_property_batches_for_label(
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

/// One candidate scan: the store and execution it reads, the source vertex it expands from, and the order it walks edges in.
///
/// Five values that opened every candidate entry, so each of them repeated the same five arguments before saying anything about what it
/// was looking for.
#[derive(Clone, Copy)]
pub(crate) struct CandidateScan<'a> {
    pub(crate) store: &'a GraphStore,
    pub(crate) execution: &'a GqlExecutionContext,
    pub(crate) src_id: VertexId,
    pub(crate) direction: EdgeDirection,
    pub(crate) sequence_order: EdgeSequenceOrder,
}

/// The narrowing a candidate scan applies: an indexed equality probe, the two inline predicates and the parameter map they read.
///
/// The two predicates are different types but both `Option<&...>`, and the equality probe is a third - so as positional arguments a call
/// site could hand the vector predicate where the property predicate belongs.
#[derive(Clone, Copy)]
pub(crate) struct CandidateFilters<'a> {
    pub(crate) indexed_edge_equality: Option<&'a (Str, ScanValue)>,
    pub(crate) edge_inline_property_predicate: Option<&'a EdgeInlinePropertyPredicate>,
    pub(crate) edge_inline_vector_predicate: Option<&'a EdgeInlineVectorPredicate>,
    pub(crate) parameters: &'a BTreeMap<String, Value>,
}

/// Expand with index/inline-property-bytes/vector predicates decomposed across labels in `label_expr`.
///
/// Returns `true` when fusion ran (including zero matches). Returns `false` when the caller
/// should use the generic `expand_candidates_into` path.
pub(crate) fn expand_candidates_with_label_expr_fusion_into(
    scan: CandidateScan<'_>,
    filters: CandidateFilters<'_>,
    label_expr: &LabelExpr,
    out: &mut Vec<ExpandCandidate>,
) -> Result<bool, PlanQueryError> {
    let CandidateScan {
        store,
        execution,
        src_id,
        direction,
        sequence_order,
    } = scan;
    let CandidateFilters {
        indexed_edge_equality,
        edge_inline_property_predicate,
        edge_inline_vector_predicate,
        parameters,
    } = filters;
    let has_predicate = indexed_edge_equality.is_some()
        || edge_inline_property_predicate.is_some()
        || edge_inline_vector_predicate.is_some();
    if !has_predicate {
        return Ok(false);
    }
    let label_ids = fusion_edge_label_ids_for_expr(execution, label_expr)
        .unwrap_or_else(|| catalog_edge_label_ids_for_predicate_fusion(execution));
    if label_ids.is_empty() {
        return Ok(false);
    }
    for label_id in label_ids {
        expand_candidates_into(
            CandidateScan {
                store,
                execution,
                src_id,
                direction,
                sequence_order,
            },
            CandidateFilters {
                indexed_edge_equality,
                edge_inline_property_predicate,
                edge_inline_vector_predicate,
                parameters,
            },
            Some(label_id),
            out,
        )?;
    }
    Ok(true)
}

pub(crate) fn expand_candidates_for_expand_op_into(
    scan: CandidateScan<'_>,
    filters: CandidateFilters<'_>,
    edge_label_id: Option<EdgeLabelId>,
    label_expr: Option<&LabelExpr>,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let CandidateScan {
        store,
        execution,
        src_id,
        direction,
        sequence_order,
    } = scan;
    let CandidateFilters {
        indexed_edge_equality,
        edge_inline_property_predicate,
        edge_inline_vector_predicate,
        parameters,
    } = filters;
    let composite_directions: Option<&'static [EdgeDirection]> = match direction {
        EdgeDirection::LeftOrUndirected => {
            Some(&[EdgeDirection::PointingLeft, EdgeDirection::Undirected])
        }
        EdgeDirection::UndirectedOrRight => {
            Some(&[EdgeDirection::Undirected, EdgeDirection::PointingRight])
        }
        EdgeDirection::LeftOrRight => {
            Some(&[EdgeDirection::PointingLeft, EdgeDirection::PointingRight])
        }
        EdgeDirection::AnyDirection => Some(&[
            EdgeDirection::PointingLeft,
            EdgeDirection::Undirected,
            EdgeDirection::PointingRight,
        ]),
        EdgeDirection::PointingRight | EdgeDirection::PointingLeft | EdgeDirection::Undirected => {
            None
        }
    };
    if let Some(directions) = composite_directions {
        for direction in directions {
            expand_candidates_for_expand_op_into(
                CandidateScan {
                    store,
                    execution,
                    src_id,
                    direction: *direction,
                    sequence_order,
                },
                CandidateFilters {
                    indexed_edge_equality,
                    edge_inline_property_predicate,
                    edge_inline_vector_predicate,
                    parameters,
                },
                edge_label_id,
                label_expr,
                out,
            )?;
        }
        return Ok(());
    }
    if edge_label_id.is_none()
        && let Some(expr) = label_expr
        && expand_candidates_with_label_expr_fusion_into(
            CandidateScan {
                store,
                execution,
                src_id,
                direction,
                sequence_order,
            },
            CandidateFilters {
                indexed_edge_equality,
                edge_inline_property_predicate,
                edge_inline_vector_predicate,
                parameters,
            },
            expr,
            out,
        )?
    {
        return Ok(());
    }
    expand_candidates_into(
        CandidateScan {
            store,
            execution,
            src_id,
            direction,
            sequence_order,
        },
        CandidateFilters {
            indexed_edge_equality,
            edge_inline_property_predicate,
            edge_inline_vector_predicate,
            parameters,
        },
        edge_label_id,
        out,
    )
}

pub(crate) fn expand_candidates_into(
    scan: CandidateScan<'_>,
    filters: CandidateFilters<'_>,
    edge_label_id: Option<EdgeLabelId>,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let CandidateScan {
        store,
        execution,
        src_id,
        direction,
        sequence_order,
    } = scan;
    let CandidateFilters {
        indexed_edge_equality,
        edge_inline_property_predicate,
        edge_inline_vector_predicate,
        parameters,
    } = filters;
    let indexed = indexed_edge_equality.map(|(property, value)| (property.as_ref(), value));
    if let Some((property, scan_value)) = indexed
        && expand_candidates_via_equality_index(
            CandidateScan {
                store,
                execution,
                src_id,
                direction,
                sequence_order,
            },
            edge_label_id,
            property,
            scan_value,
            parameters,
            out,
        )?
    {
        return Ok(());
    }
    if let Some(edge_inline_property_predicate) = edge_inline_property_predicate {
        let Some(edge_label_id) = edge_label_id else {
            return Ok(());
        };
        let Some(predicate) = PreparedEdgeInlinePropertyPredicate::prepare(
            execution.resolved_labels.as_ref(),
            edge_label_id,
            edge_inline_property_predicate,
            parameters,
        )?
        else {
            return Ok(());
        };
        expand_candidates_matching_edge_inline_property_into(
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
    if let Some(edge_inline_vector_predicate) = edge_inline_vector_predicate {
        let Some(edge_label_id) = edge_label_id else {
            return Ok(());
        };
        let Some(predicate) = PreparedEdgeInlineVectorThreshold::prepare(
            execution.resolved_labels.as_ref(),
            edge_label_id,
            edge_inline_vector_predicate,
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
                            execution,
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
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(&edge)
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
                            execution,
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
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(&edge)
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
                            execution,
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
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(&edge)
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
                store.for_each_directed_out_edges_for_label_with_inline_property_bytes(
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
                store.for_each_directed_in_edges_for_label_with_inline_property_bytes(
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
    scan: CandidateScan<'_>,
    edge_label_id: Option<EdgeLabelId>,
    property: &str,
    scan_value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
    out: &mut Vec<ExpandCandidate>,
) -> Result<bool, PlanQueryError> {
    let CandidateScan {
        store,
        execution,
        src_id,
        direction,
        sequence_order,
    } = scan;
    let Some(property_id) = execution.resolved_property_id(property) else {
        return Ok(false);
    };
    // An IN-list bound unions one equality probe per element; every other scan
    // value probes exactly one bound. Elements resolve independently so a
    // missing parameter fails closed instead of narrowing the candidates.
    let probes = resolve_edge_equality_probe_payloads(scan_value, parameters)?;
    let expected_any: Vec<Vec<u8>> = probes.into_iter().flatten().collect();
    if expected_any.is_empty() {
        out.clear();
        return Ok(true);
    }
    let postings = if expected_any.len() == 1 {
        edge_lookup::lookup_edge_equal_local_sync(
            None,
            property_id,
            &expected_any[0],
            edge_label_id.map(|id| id.raw()),
        )?
    } else {
        edge_lookup::lookup_edge_equal_union_local(
            None,
            property_id,
            &expected_any,
            edge_label_id.map(|id| id.raw()),
        )?
    };
    if postings.is_empty() {
        return Ok(false);
    }

    let mut out_slots: BTreeSet<(u16, BucketEntryPosition)> = BTreeSet::new();
    let mut in_slots: BTreeSet<(u32, u16, BucketEntryPosition)> = BTreeSet::new();
    for posting in &postings {
        if posting.owner_vertex_id == src_id {
            out_slots.insert((
                posting.label_id,
                BucketEntryPosition::new(posting.slot_index),
            ));
        }
        in_slots.insert((
            u32::from(posting.owner_vertex_id),
            posting.label_id,
            BucketEntryPosition::new(posting.slot_index),
        ));
    }

    let mut error = None;
    match direction {
        EdgeDirection::PointingRight => {
            // The equality-index postings name forward-owner slots, but the slot-targeted
            // reader restores topology only — inline property bytes come from the batch
            // adjacency walk. Scan this vertex's labeled adjacency once and keep the posted
            // slots so projected inline reads see their bytes (mirrors PointingLeft).
            let posted: BTreeSet<(u16, u32)> = out_slots
                .iter()
                .filter(|(label_id, _)| {
                    edge_label_id
                        .map(|label| label.pack(EdgeDirectedness::Directed).raw() == *label_id)
                        .unwrap_or(true)
                })
                .map(|(label_id, position)| (*label_id, position.raw()))
                .collect();
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
                    if !posted.contains(&(edge.label_id, edge.edge_slot_index.raw())) {
                        return;
                    }
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(&edge)
                        && let Err(err) =
                            push_expand_candidate(out, store, src_id, direction, edge_dst, edge)
                    {
                        error = Some(err);
                    }
                },
            )?;
        }
        // Postings record forward-owner slots; reverse/undirected expand still needs a full
        // adjacency scan plus canonical handle matching to locate incoming edges at `src_id`.
        EdgeDirection::PointingLeft => {
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
                    let canonical = store.canonical_edge_handle(EdgeHandle {
                        owner_vertex_id: src_id,
                        label_id: LaraLabelId::from_raw(edge.label_id),
                        slot_index: edge.edge_slot_index.raw().into(),
                    });
                    if !in_slots.contains(&(
                        u32::from(canonical.owner_vertex_id),
                        canonical.label_id.raw(),
                        canonical.slot_index,
                    )) {
                        return;
                    }
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(&edge)
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
                sequence_order,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    let canonical = store.canonical_edge_handle(EdgeHandle {
                        owner_vertex_id: src_id,
                        label_id: LaraLabelId::from_raw(edge.label_id),
                        slot_index: edge.edge_slot_index.raw().into(),
                    });
                    if !in_slots.contains(&(
                        u32::from(canonical.owner_vertex_id),
                        canonical.label_id.raw(),
                        canonical.slot_index,
                    )) {
                        return;
                    }
                    if let Ok(Some(edge_dst)) = ExpandDst::from_edge(&edge)
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
