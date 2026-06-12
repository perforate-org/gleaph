use std::collections::BTreeMap;

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::plan::{
    AggregateSpec, EdgePayloadPredicate, EdgeVectorPredicate, PlanOp, ScanValue, Str,
};
use gleaph_graph_kernel::entry::{Edge, PreparedWeightDecoder};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::traits::CsrVertexTombstone;

use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::context::QueryExprEvaluator;
use crate::plan::query::executor::expand::{
    EdgeEqualityStreamFilter, ExpandDst, build_expanded_row, collect_var_len_expand_rows,
    csr_offset_fast_path_for_expand, edge_binding_for_scanned_expand,
    edge_binding_matches_label_expr, edge_equality_stream_filter, edge_matches_label_expr,
    edge_matches_stream_filter, expand_accepts_remote_dst, expand_candidates_for_expand_op_into,
    expand_dst_matches_prebound_vertex, visit_csr_expand_fast_path,
};
use crate::plan::query::executor::{
    EdgeSequenceOrder, PlanBinding, dst_filter_is_dst_vertex_only, ensure_simple_expand,
    ensure_var_len_expand, limit_value, project_row, row_matches_all,
    vertex_row_matches_dst_filters,
};
use crate::plan::query::row::PlanRow;

struct LimitedRows {
    offset_remaining: usize,
    take_remaining: usize,
    rows: Vec<PlanRow>,
}

impl LimitedRows {
    fn new(offset: usize, count: usize) -> Self {
        Self {
            offset_remaining: offset,
            take_remaining: count,
            rows: Vec::new(),
        }
    }

    fn is_done(&self) -> bool {
        self.take_remaining == 0
    }

    fn push(&mut self, row: PlanRow) -> bool {
        if self.offset_remaining > 0 {
            self.offset_remaining -= 1;
            return false;
        }
        if self.take_remaining == 0 {
            return true;
        }
        self.rows.push(row);
        self.take_remaining -= 1;
        self.take_remaining == 0
    }
}

fn streaming_ops_preserve_row_cardinality_after(ops: &[PlanOp], start: usize) -> bool {
    let mut i = start;
    while i < ops.len() {
        match &ops[i] {
            PlanOp::Project { distinct, .. } if !distinct => i += 1,
            PlanOp::Let { .. } => i += 1,
            _ => return false,
        }
    }
    true
}

pub(crate) fn limited_streaming_prefix_limit_idx(
    ops: &[PlanOp],
    start_idx: usize,
) -> Option<usize> {
    for (idx, op) in ops.iter().enumerate().skip(start_idx) {
        match op {
            PlanOp::Limit { count: Some(_), .. } => return Some(idx),
            PlanOp::Limit { count: None, .. } => return None,
            op if streaming_prefix_op_supported(op) => {}
            _ => return None,
        }
    }
    None
}

fn streaming_prefix_op_supported(op: &PlanOp) -> bool {
    match op {
        PlanOp::NodeScan { .. }
        | PlanOp::PropertyFilter { .. }
        | PlanOp::Let { .. }
        | PlanOp::Filter { .. }
        | PlanOp::Expand { .. }
        | PlanOp::ExpandFilter { .. } => true,
        PlanOp::Project { distinct, .. } => !distinct,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_limited_streaming_prefix(
    store: &GraphStore,
    ops: &[PlanOp],
    initial_rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    execution: &GqlExecutionContext,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    aggregate_specs: Option<&[AggregateSpec]>,
) -> Result<super::LimitedStreamingPrefixResult, PlanQueryError> {
    let Some((PlanOp::Limit { count, offset }, streaming_ops)) = ops.split_last() else {
        return Ok(super::LimitedStreamingPrefixResult {
            rows: initial_rows,
            clears_active_aggregate: false,
        });
    };
    let evaluator = QueryExprEvaluator {
        store,
        parameters,
        aggregate_specs,
        caller,
        resolved_labels: execution.resolved_labels.as_ref(),
        resolved_properties: execution.resolved_properties.as_ref(),
        gleaph_weight_decoders,
    };
    let offset = match offset {
        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
        None => 0,
    };
    let count = match count {
        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
        None => {
            return Ok(super::LimitedStreamingPrefixResult {
                rows: initial_rows,
                clears_active_aggregate: false,
            });
        }
    };
    let mut sink = LimitedRows::new(offset, count);
    let mut clears_active_aggregate = false;
    if sink.is_done() {
        return Ok(super::LimitedStreamingPrefixResult {
            rows: sink.rows,
            clears_active_aggregate,
        });
    }

    for row in initial_rows {
        if stream_row_through_ops(
            store,
            streaming_ops,
            0,
            row,
            parameters,
            caller,
            execution,
            gleaph_weight_decoders,
            &evaluator,
            &mut sink,
            &mut clears_active_aggregate,
        )? {
            break;
        }
    }

    Ok(super::LimitedStreamingPrefixResult {
        rows: sink.rows,
        clears_active_aggregate,
    })
}

#[allow(clippy::too_many_arguments)]
fn stream_row_through_ops(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    execution: &GqlExecutionContext,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
    let Some(op) = ops.get(op_idx) else {
        return Ok(sink.push(row));
    };
    match op {
        PlanOp::NodeScan {
            variable,
            label,
            property_projection: _,
        } => stream_node_scan(
            store,
            ops,
            op_idx,
            row,
            variable,
            label.as_deref(),
            execution,
            parameters,
            caller,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        ),
        PlanOp::PropertyFilter { predicates, .. } => {
            if row_matches_all(evaluator, &row, predicates)? {
                stream_row_through_ops(
                    store,
                    ops,
                    op_idx + 1,
                    row,
                    parameters,
                    caller,
                    execution,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            } else {
                Ok(false)
            }
        }
        PlanOp::Let { bindings } => {
            let mut row = row;
            for binding in bindings {
                let value = evaluator.eval_expr(&row, &binding.value)?;
                row.insert(binding.variable.clone(), PlanBinding::Value(value));
            }
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                row,
                parameters,
                caller,
                execution,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        }
        PlanOp::Filter { condition } => {
            if row_matches_all(evaluator, &row, std::slice::from_ref(condition))? {
                stream_row_through_ops(
                    store,
                    ops,
                    op_idx + 1,
                    row,
                    parameters,
                    caller,
                    execution,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            } else {
                Ok(false)
            }
        }
        PlanOp::Project { columns, distinct } => {
            debug_assert!(!distinct);
            let projected = project_row(evaluator, &row, columns)?;
            *clears_active_aggregate = true;
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                projected,
                parameters,
                caller,
                execution,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        }
        PlanOp::Expand {
            src,
            edge,
            dst,
            direction,
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            edge_payload_predicate,
            edge_vector_predicate,
            edge_property_projection,
            dst_property_projection,
            hop_aux_binding,
            emit_edge_binding,
            near_group_var,
            far_group_var,
            path_var,
            emit_path_binding,
        } => {
            if let Some(bounds) = var_len {
                ensure_var_len_expand()?;
                stream_var_len_expand(
                    store,
                    ops,
                    op_idx,
                    row,
                    parameters,
                    src,
                    edge,
                    dst,
                    *direction,
                    label.as_deref(),
                    label_expr.as_ref(),
                    execution,
                    bounds,
                    &[],
                    *emit_edge_binding,
                    hop_aux_binding.as_ref(),
                    near_group_var.as_ref(),
                    far_group_var.as_ref(),
                    path_var.as_ref(),
                    *emit_path_binding,
                    indexed_edge_equality.as_ref(),
                    edge_payload_predicate.as_ref(),
                    edge_vector_predicate.as_ref(),
                    edge_property_projection.as_deref(),
                    dst_property_projection.as_deref(),
                    caller,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            } else {
                ensure_simple_expand(var_len)?;
                stream_expand(
                    store,
                    ops,
                    op_idx,
                    row,
                    parameters,
                    src,
                    edge,
                    dst,
                    *direction,
                    label.as_deref(),
                    label_expr.as_ref(),
                    execution,
                    EdgeSequenceOrder::Descending,
                    &[],
                    *emit_edge_binding,
                    hop_aux_binding.as_ref(),
                    indexed_edge_equality.as_ref(),
                    edge_payload_predicate.as_ref(),
                    edge_vector_predicate.as_ref(),
                    edge_property_projection.as_deref(),
                    dst_property_projection.as_deref(),
                    caller,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            }
        }
        PlanOp::ExpandFilter {
            src,
            edge,
            dst,
            direction,
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            edge_payload_predicate,
            edge_vector_predicate,
            dst_filter,
            edge_property_projection,
            dst_property_projection,
            hop_aux_binding,
            emit_edge_binding,
            near_group_var,
            far_group_var,
            path_var,
            emit_path_binding,
        } => {
            if let Some(bounds) = var_len {
                ensure_var_len_expand()?;
                stream_var_len_expand(
                    store,
                    ops,
                    op_idx,
                    row,
                    parameters,
                    src,
                    edge,
                    dst,
                    *direction,
                    label.as_deref(),
                    label_expr.as_ref(),
                    execution,
                    bounds,
                    dst_filter,
                    *emit_edge_binding,
                    hop_aux_binding.as_ref(),
                    near_group_var.as_ref(),
                    far_group_var.as_ref(),
                    path_var.as_ref(),
                    *emit_path_binding,
                    indexed_edge_equality.as_ref(),
                    edge_payload_predicate.as_ref(),
                    edge_vector_predicate.as_ref(),
                    edge_property_projection.as_deref(),
                    dst_property_projection.as_deref(),
                    caller,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            } else {
                ensure_simple_expand(var_len)?;
                stream_expand(
                    store,
                    ops,
                    op_idx,
                    row,
                    parameters,
                    src,
                    edge,
                    dst,
                    *direction,
                    label.as_deref(),
                    label_expr.as_ref(),
                    execution,
                    EdgeSequenceOrder::Descending,
                    dst_filter,
                    *emit_edge_binding,
                    hop_aux_binding.as_ref(),
                    indexed_edge_equality.as_ref(),
                    edge_payload_predicate.as_ref(),
                    edge_vector_predicate.as_ref(),
                    edge_property_projection.as_deref(),
                    dst_property_projection.as_deref(),
                    caller,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            }
        }
        _ => unreachable!("limited streaming prefix only contains supported operators"),
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_node_scan(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    variable: &Str,
    label: Option<&str>,
    execution: &GqlExecutionContext,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
    let label_id = match label {
        Some(label) => execution
            .resolved_vertex_label_id(label)
            .map(Some)
            .ok_or_else(|| PlanQueryError::MissingResolvedLabel {
                namespace: "node",
                name: label.to_owned(),
            })?,
        None => None,
    };

    for raw in 0..u32::from(store.vertex_count()) {
        #[cfg(test)]
        super::NODE_SCAN_VISITS.with(|visits| visits.set(visits.get() + 1));
        let vertex_id = VertexId::from(raw);
        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone() {
            continue;
        }
        if let Some(filter) = label_id
            && !store.vertex_has_label(vertex_id, vertex, filter)
        {
            continue;
        }
        let scanned = row.fork([(variable.as_ref(), PlanBinding::Vertex(vertex_id))]);
        if stream_row_through_ops(
            store,
            ops,
            op_idx + 1,
            scanned,
            parameters,
            caller,
            execution,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn local_vertex_binding_for_limited_streaming_expand(
    row: &PlanRow,
    variable: &str,
) -> Result<Option<VertexId>, PlanQueryError> {
    match row.get(variable) {
        Some(PlanBinding::Value(Value::Null)) => Ok(None),
        Some(PlanBinding::Vertex(vertex_id)) => Ok(Some(*vertex_id)),
        Some(PlanBinding::RemoteVertex(_)) => Err(PlanQueryError::UnsupportedOp(
            super::LIMITED_STREAMING_REMOTE_EXPAND_SOURCE,
        )),
        Some(_) | None => Err(PlanQueryError::MissingBinding {
            variable: variable.to_owned(),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_var_len_expand(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    parameters: &BTreeMap<String, Value>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&str>,
    label_expr: Option<&LabelExpr>,
    execution: &GqlExecutionContext,
    var_len: &gleaph_gql_planner::plan::VarLenSpec,
    dst_filter: &[Expr],
    emit_edge_binding: bool,
    hop_aux_binding: Option<&Str>,
    near_group_var: Option<&Str>,
    far_group_var: Option<&Str>,
    path_var: Option<&Str>,
    emit_path_binding: bool,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_payload_predicate: Option<&EdgePayloadPredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
    if row
        .get(src.as_ref())
        .is_some_and(|binding| matches!(binding, PlanBinding::RemoteVertex(_)))
    {
        return Err(PlanQueryError::UnsupportedOp("Expand.var_len.remote"));
    }
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
    let Some(src_id) = local_vertex_binding_for_limited_streaming_expand(&row, src.as_ref())?
    else {
        return Ok(false);
    };
    let mut expanded_rows = Vec::new();
    collect_var_len_expand_rows(
        store,
        &row,
        src_id,
        edge,
        dst,
        direction,
        label_id,
        label_expr,
        execution,
        var_len,
        dst_filter,
        emit_edge_binding,
        hop_aux_binding,
        near_group_var,
        far_group_var,
        path_var,
        emit_path_binding,
        parameters,
        indexed_edge_equality,
        edge_payload_predicate,
        edge_vector_predicate,
        edge_property_projection,
        dst_property_projection,
        evaluator,
        &mut expanded_rows,
    )?;
    let mut any = false;
    for expanded in expanded_rows {
        if stream_row_through_ops(
            store,
            ops,
            op_idx + 1,
            expanded,
            parameters,
            caller,
            execution,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        )? {
            any = true;
        }
    }
    Ok(any)
}

#[allow(clippy::too_many_arguments)]
fn stream_expand(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    parameters: &BTreeMap<String, Value>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&str>,
    label_expr: Option<&LabelExpr>,
    execution: &GqlExecutionContext,
    sequence_order: EdgeSequenceOrder,
    dst_filter: &[Expr],
    emit_edge_binding: bool,
    hop_aux_binding: Option<&Str>,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_payload_predicate: Option<&EdgePayloadPredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
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

    let Some(src_id) = local_vertex_binding_for_limited_streaming_expand(&row, src.as_ref())?
    else {
        return Ok(false);
    };
    let dst_only_prefilter = dst_filter_is_dst_vertex_only(dst_filter, dst.as_ref());
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let hop_aux_key = hop_aux_binding.map(|name| name.as_ref());
    let dst_key = dst.to_string();
    let csr_expand_fast_path = (edge_payload_predicate.is_none()
        && edge_vector_predicate.is_none())
    .then(|| csr_offset_fast_path_for_expand(direction, label_id, sequence_order))
    .flatten();

    let csr_offset_fast_path = (indexed_edge_equality.is_none()
        && edge_payload_predicate.is_none()
        && edge_vector_predicate.is_none()
        && dst_filter.is_empty()
        && !matches!(
            row.get(dst.as_ref()),
            Some(PlanBinding::Vertex(_)) | Some(PlanBinding::RemoteVertex(_))
        )
        && streaming_ops_preserve_row_cardinality_after(ops, op_idx + 1))
    .then_some(csr_expand_fast_path)
    .flatten();

    if let Some(fast_path) = csr_offset_fast_path {
        let mut offset_slot = sink.offset_remaining;
        let mut visit = |edge: Edge| {
            #[cfg(test)]
            super::EDGE_STREAM_VISITS.with(|visits| visits.set(visits.get() + 1));
            // `skip_then_visit_each_*` applies the global OFFSET inside the CSR iterator; clear
            // the sink-side skip before downstream `LimitedRows::push`.
            sink.offset_remaining = 0;
            if let Some(expr) = label_expr
                && !edge_matches_label_expr(execution, expr, &edge)
            {
                return Ok(false);
            }
            let Some(edge_dst) = ExpandDst::from_edge(&edge)? else {
                return Ok(false);
            };
            if !expand_accepts_remote_dst(dst_only_prefilter, dst_property_projection)
                && !edge_dst.requires_local_vertex_data()
            {
                return Ok(false);
            }
            let edge_binding = edge_binding_for_scanned_expand(store, src_id, direction, edge)?;
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
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                expanded,
                parameters,
                caller,
                execution,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        };
        let res =
            visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
        sink.offset_remaining = offset_slot;
        return match res {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        };
    }

    if let Some(fast_path) = csr_expand_fast_path {
        let edge_equality_filter =
            edge_equality_stream_filter(execution, indexed_edge_equality, parameters)?;
        if matches!(edge_equality_filter, EdgeEqualityStreamFilter::NoMatches) {
            return Ok(false);
        }
        let mut offset_slot = 0;
        let mut visit = |edge: Edge| {
            #[cfg(test)]
            super::EDGE_STREAM_VISITS.with(|visits| visits.set(visits.get() + 1));
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
            let edge_binding = edge_binding_for_scanned_expand(store, src_id, direction, edge)?;
            if !edge_matches_stream_filter(
                store,
                &edge_equality_filter,
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
            if !dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
                return Ok(false);
            }
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                expanded,
                parameters,
                caller,
                execution,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        };
        let res =
            visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
        return match res {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        };
    }

    let mut candidates = Vec::new();
    expand_candidates_for_expand_op_into(
        store,
        execution,
        src_id,
        direction,
        label_id,
        label_expr,
        EdgeSequenceOrder::Descending,
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
        if !dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
            continue;
        }
        if stream_row_through_ops(
            store,
            ops,
            op_idx + 1,
            expanded,
            parameters,
            caller,
            execution,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}
