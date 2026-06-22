use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::plan::{EdgePayloadPredicate, EdgeVectorPredicate, ScanValue, Str};
use gleaph_graph_kernel::entry::{Edge, EdgeLabelId};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::labeled::LabeledEdgePayloadBatchScratch;

use super::candidates::{expand_candidates_for_expand_op_into, expand_vector_dst_only_rows_into};
use super::label_expr::{edge_binding_matches_label_expr, edge_matches_label_expr};
use super::predicates::PreparedEdgeVectorThreshold;
use super::{
    EdgeEqualityStreamFilter, ExpandDst, build_expanded_row, csr_offset_fast_path_for_expand,
    edge_binding_for_scanned_expand, edge_equality_stream_filter, edge_matches_stream_filter,
    expand_accepts_remote_dst, visit_csr_expand_fast_path,
};
use crate::federation::{TraversalExpandSource, resolve_traversal_expand_source};
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::context::ExecuteCtx;
use crate::plan::query::executor::{
    EdgeSequenceOrder, PlanBinding, dst_filter_is_dst_vertex_only, row_matches_all,
    vertex_row_matches_dst_filters,
};
use crate::plan::query::row::PlanRow;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_expand(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&str>,
    label_expr: Option<&LabelExpr>,
    execution: &crate::gql_execution_context::GqlExecutionContext,
    sequence_order: EdgeSequenceOrder,
    dst_filter: &[Expr],
    emit_edge_binding: bool,
    hop_aux_binding: Option<&Str>,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_payload_predicate: Option<&EdgePayloadPredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let store = ctx.store;
    let parameters = ctx.parameters;
    let caller = ctx.caller();
    let gleaph_weight_decoders = ctx.gleaph_weight_decoders;
    let label_id = match label {
        Some(label) => execution
            .resolved_edge_label_id(label)
            .map(Some)
            .ok_or_else(|| PlanQueryError::MissingResolvedLabel {
                namespace: "edge",
                name: label.to_owned(),
            })?,
        None => None,
    };

    let evaluator = ctx.expr_evaluator(None);
    let dst_only_prefilter = dst_filter_is_dst_vertex_only(dst_filter, dst.as_ref());
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let hop_aux_key = hop_aux_binding.map(|name| name.as_ref());
    let dst_key = dst.to_string();
    let csr_expand_fast_path = (edge_payload_predicate.is_none()
        && edge_vector_predicate.is_none())
    .then(|| csr_offset_fast_path_for_expand(direction, label_id, sequence_order))
    .flatten();
    let prepared_vector_dst_only_predicate = prepare_vector_dst_only_expand_predicate(
        execution.resolved_labels.as_ref(),
        label_id,
        direction,
        emit_edge_binding,
        hop_aux_binding,
        indexed_edge_equality,
        edge_payload_predicate,
        edge_vector_predicate,
        edge_property_projection,
        parameters,
    )?;
    let edge_equality_filter = if csr_expand_fast_path.is_some() {
        let filter = edge_equality_stream_filter(
            ctx.index,
            execution,
            indexed_edge_equality,
            parameters,
            label_id.map(|id| id.raw()),
        )?;
        if matches!(filter, EdgeEqualityStreamFilter::NoMatches) {
            return Ok(Vec::new());
        }
        Some(filter)
    } else {
        None
    };
    let mut out = Vec::with_capacity(rows.len());
    let mut candidates = Vec::new();
    let mut vector_batch_scratch = LabeledEdgePayloadBatchScratch::default();
    let mut vector_matches = Vec::new();
    for row in rows {
        match resolve_traversal_expand_source(store, row.get(src.as_ref()), direction).await? {
            None => continue,
            Some(TraversalExpandSource::LocalCsr(src_id)) => {
                if let Some(fast_path) = csr_expand_fast_path {
                    let mut offset_slot = 0;
                    let mut visit = |edge: Edge| {
                        if let Some(expr) = label_expr
                            && !edge_matches_label_expr(execution, expr, &edge)
                        {
                            return Ok(false);
                        }
                        let Some(edge_dst) = ExpandDst::from_edge(&edge)? else {
                            return Ok(false);
                        };
                        let label_id = edge.label_id;
                        let slot_index = edge.edge_slot_index;
                        let edge_binding =
                            edge_binding_for_scanned_expand(store, src_id, direction, edge)?;
                        if !edge_matches_stream_filter(
                            store,
                            edge_equality_filter
                                .as_ref()
                                .expect("filter exists with fast path"),
                            direction,
                            edge_binding.handle.owner_vertex_id,
                            LaraLabelId::from_raw(label_id),
                            slot_index,
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
                                    &evaluator.element_id_key,
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
                        } else if !expand_accepts_remote_dst(
                            dst_only_prefilter,
                            dst_property_projection,
                        ) {
                            return Ok(false);
                        }
                        let expanded = build_expanded_row(
                            None,
                            store,
                            execution,
                            &row,
                            edge_key.as_deref(),
                            hop_aux_key,
                            dst_key.as_str(),
                            edge_dst,
                            edge_binding,
                            edge_property_projection,
                            dst_property_projection,
                        )?;
                        if !dst_only_prefilter
                            && !row_matches_all(&evaluator, &expanded, dst_filter)?
                        {
                            return Ok(false);
                        }
                        out.push(expanded);
                        Ok(false)
                    };
                    let res = visit_csr_expand_fast_path(
                        store,
                        src_id,
                        fast_path,
                        &mut offset_slot,
                        &mut visit,
                    );
                    match res {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => return Err(e),
                        Err(e) => return Err(e.into()),
                    }
                    continue;
                }
                if let Some((edge_label_id, predicate)) =
                    prepared_vector_dst_only_predicate.as_ref()
                {
                    expand_vector_dst_only_rows_into(
                        store,
                        execution,
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
                expand_candidates_for_expand_op_into(
                    store,
                    execution,
                    src_id,
                    direction,
                    label_id,
                    label_expr,
                    sequence_order,
                    indexed_edge_equality,
                    edge_payload_predicate,
                    edge_vector_predicate,
                    parameters,
                    &mut candidates,
                )?;
                for (edge_dst, edge_binding) in candidates.iter().cloned() {
                    if let Some(expr) = label_expr
                        && !edge_binding_matches_label_expr(execution, expr, &edge_binding)
                    {
                        continue;
                    }
                    if !expand_dst_matches_prebound_vertex(&row, dst, edge_dst) {
                        continue;
                    }
                    if let ExpandDst::Local(dst_id) = edge_dst {
                        if dst_only_prefilter
                            && !vertex_row_matches_dst_filters(
                                store,
                                &evaluator.element_id_key,
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
                    } else if !expand_accepts_remote_dst(
                        dst_only_prefilter,
                        dst_property_projection,
                    ) {
                        continue;
                    }
                    let expanded = build_expanded_row(
                        None,
                        store,
                        execution,
                        &row,
                        edge_key.as_deref(),
                        hop_aux_key,
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
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn prepare_vector_dst_only_expand_predicate(
    resolved_labels: Option<&gleaph_graph_kernel::plan_exec::ResolvedLabelTable>,
    label_id: Option<EdgeLabelId>,
    direction: EdgeDirection,
    emit_edge_binding: bool,
    hop_aux_binding: Option<&Str>,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_payload_predicate: Option<&EdgePayloadPredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    edge_property_projection: Option<&[Str]>,
    parameters: &BTreeMap<String, Value>,
) -> Result<Option<(EdgeLabelId, PreparedEdgeVectorThreshold)>, PlanQueryError> {
    if emit_edge_binding
        || hop_aux_binding.is_some()
        || indexed_edge_equality.is_some()
        || edge_payload_predicate.is_some()
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
        resolved_labels,
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
