use std::collections::BTreeMap;

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql_planner::plan::{AggregateSpec, PlanOp};
use gleaph_graph_kernel::entry::Edge;
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
use crate::plan::query::executor::expand::{ExpandCommon, expand_common};
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

pub(crate) fn execute_limited_streaming_prefix(
    store: &GraphStore,
    ops: &[PlanOp],
    initial_rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    execution: &GqlExecutionContext,
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
        element_id_key: crate::element_id_encoding::resolve_or_host_fixture(
            execution.element_id_encoding_key(),
        ),
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

    let at = OpAt {
        ops: streaming_ops,
        op_idx: 0,
    };
    let mut out = StreamOut {
        sink: &mut sink,
        clears_active_aggregate: &mut clears_active_aggregate,
    };
    for row in initial_rows {
        if stream_row_through_ops(at, &mut out, row, execution, &evaluator)? {
            break;
        }
    }
    let StreamOut { .. } = out;

    Ok(super::LimitedStreamingPrefixResult {
        rows: sink.rows,
        clears_active_aggregate,
    })
}

/// The op list plus the position the walk is at, passed **by value** so advancing cannot leak between calls.
///
/// A previous attempt bundled the position into a shared `&mut` cursor and advanced it in place; the walk then reused a position
/// another call had moved, and three LIMIT/OFFSET tests caught it. A copy with an explicit `next` makes that impossible: every call
/// sees the position it was given.
#[derive(Clone, Copy)]
struct OpAt<'a> {
    ops: &'a [PlanOp],
    op_idx: usize,
}

impl<'a> OpAt<'a> {
    fn next(self) -> Self {
        Self {
            ops: self.ops,
            op_idx: self.op_idx + 1,
        }
    }

    fn current(self) -> Option<&'a PlanOp> {
        self.ops.get(self.op_idx)
    }
}

/// Where a streaming walk's rows go, and whether the active aggregate was cleared.
///
/// These two travel together through every level of the walk and are the only `&mut` state it carries, so they are one value.
struct StreamOut<'a> {
    sink: &'a mut LimitedRows,
    clears_active_aggregate: &'a mut bool,
}

fn stream_row_through_ops(
    at: OpAt<'_>,
    out: &mut StreamOut<'_>,
    row: PlanRow,
    execution: &GqlExecutionContext,
    evaluator: &QueryExprEvaluator<'_>,
) -> Result<bool, PlanQueryError> {
    // The op now comes from the position value, and the store/parameters/caller from the evaluator - the dispatcher used to take
    // all of them a second time.
    let Some(op) = at.current() else {
        return Ok(out.sink.push(row));
    };
    match op {
        PlanOp::NodeScan { .. } => stream_node_scan(at, out, row, execution, evaluator),
        PlanOp::PropertyFilter { predicates, .. } => {
            if row_matches_all(evaluator, &row, predicates)? {
                stream_row_through_ops(at.next(), out, row, execution, evaluator)
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
            stream_row_through_ops(at.next(), out, row, execution, evaluator)
        }
        PlanOp::Filter { condition } => {
            if row_matches_all(evaluator, &row, std::slice::from_ref(condition))? {
                stream_row_through_ops(at.next(), out, row, execution, evaluator)
            } else {
                Ok(false)
            }
        }
        PlanOp::Project { columns, distinct } => {
            debug_assert!(!distinct);
            let projected = project_row(evaluator, &row, columns)?;
            *out.clears_active_aggregate = true;
            stream_row_through_ops(at.next(), out, projected, execution, evaluator)
        }
        PlanOp::Expand { var_len, .. } => {
            if var_len.is_some() {
                ensure_var_len_expand()?;
                stream_var_len_expand(op, at, out, row, &[], execution, evaluator)
            } else {
                ensure_simple_expand(var_len)?;
                stream_expand(op, at, out, row, &[], execution, evaluator)
            }
        }
        PlanOp::ExpandFilter {
            var_len,
            dst_filter,
            ..
        } => {
            // The fused variant differs from `Expand` in exactly one dimension: its destination predicate.
            if var_len.is_some() {
                ensure_var_len_expand()?;
                stream_var_len_expand(op, at, out, row, dst_filter, execution, evaluator)
            } else {
                ensure_simple_expand(var_len)?;
                stream_expand(op, at, out, row, dst_filter, execution, evaluator)
            }
        }
        _ => unreachable!("limited streaming prefix only contains supported operators"),
    }
}

fn stream_node_scan(
    at: OpAt<'_>,
    out: &mut StreamOut<'_>,
    row: PlanRow,
    execution: &GqlExecutionContext,
    evaluator: &QueryExprEvaluator<'_>,
) -> Result<bool, PlanQueryError> {
    let op = at
        .current()
        .expect("stream_node_scan is called for an op in the list");
    let PlanOp::NodeScan {
        variable, label, ..
    } = op
    else {
        unreachable!("stream_node_scan is called for PlanOp::NodeScan")
    };

    // The evaluator owns the store; taking it again let a caller disagree with its own evaluator.
    let store = evaluator.store;
    let label = label.as_deref();
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
        if stream_row_through_ops(at.next(), out, scanned, execution, evaluator)? {
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

fn stream_var_len_expand(
    op: &PlanOp,
    at: OpAt<'_>,
    out: &mut StreamOut<'_>,
    row: PlanRow,
    dst_filter: &[Expr],
    execution: &GqlExecutionContext,
    evaluator: &QueryExprEvaluator<'_>,
) -> Result<bool, PlanQueryError> {
    let ExpandCommon {
        src,
        edge,
        dst,
        direction,
        label,
        label_expr,
        var_len,
        indexed_edge_equality,
        edge_inline_property_predicate,
        edge_inline_vector_predicate,
        edge_property_projection,
        dst_property_projection,
        hop_aux_binding,
        emit_edge_binding,
        near_group_var,
        far_group_var,
        path_var,
        emit_path_binding,
    } = expand_common(op);
    let bounds = var_len.expect("the dispatcher routes a var-length expansion here");
    let store = evaluator.store;
    let parameters = evaluator.parameters;

    // `caller` reached this function only to be handed to `stream_row_through_ops`, which now reads it from the evaluator.
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
        bounds,
        dst_filter,
        emit_edge_binding,
        hop_aux_binding,
        near_group_var,
        far_group_var,
        path_var,
        emit_path_binding,
        parameters,
        indexed_edge_equality,
        edge_inline_property_predicate,
        edge_inline_vector_predicate,
        edge_property_projection,
        dst_property_projection,
        evaluator,
        &mut expanded_rows,
    )?;
    let mut any = false;
    for expanded in expanded_rows {
        if stream_row_through_ops(at.next(), out, expanded, execution, evaluator)? {
            any = true;
        }
    }
    Ok(any)
}

fn stream_expand(
    op: &PlanOp,
    at: OpAt<'_>,
    out: &mut StreamOut<'_>,
    row: PlanRow,
    dst_filter: &[Expr],
    execution: &GqlExecutionContext,
    evaluator: &QueryExprEvaluator<'_>,
) -> Result<bool, PlanQueryError> {
    let ExpandCommon {
        src,
        edge,
        dst,
        direction,
        label,
        label_expr,
        indexed_edge_equality,
        edge_inline_property_predicate,
        edge_inline_vector_predicate,
        edge_property_projection,
        dst_property_projection,
        hop_aux_binding,
        emit_edge_binding,
        ..
    } = expand_common(op);
    let sequence_order = EdgeSequenceOrder::Ascending;
    let store = evaluator.store;
    let parameters = evaluator.parameters;
    let caller = evaluator.caller;

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
    let csr_expand_fast_path = (edge_inline_property_predicate.is_none()
        && edge_inline_vector_predicate.is_none())
    .then(|| csr_offset_fast_path_for_expand(direction, label_id, sequence_order))
    .flatten();

    let csr_offset_fast_path = (indexed_edge_equality.is_none()
        && edge_inline_property_predicate.is_none()
        && edge_inline_vector_predicate.is_none()
        && dst_filter.is_empty()
        && !matches!(
            row.get(dst.as_ref()),
            Some(PlanBinding::Vertex(_)) | Some(PlanBinding::RemoteVertex(_))
        )
        && streaming_ops_preserve_row_cardinality_after(at.ops, at.op_idx + 1))
    .then_some(csr_expand_fast_path)
    .flatten();

    if let Some(fast_path) = csr_offset_fast_path {
        let mut offset_slot = out.sink.offset_remaining;
        let mut visit = |edge: Edge| {
            #[cfg(test)]
            super::EDGE_STREAM_VISITS.with(|visits| visits.set(visits.get() + 1));
            // `skip_then_visit_each_*` applies the global OFFSET inside the CSR iterator; clear
            // the sink-side skip before downstream `LimitedRows::push`.
            out.sink.offset_remaining = 0;
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
            stream_row_through_ops(at.next(), out, expanded, execution, evaluator)
        };
        let res =
            visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
        out.sink.offset_remaining = offset_slot;
        return match res {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        };
    }

    if let Some(fast_path) = csr_expand_fast_path {
        let edge_equality_filter = edge_equality_stream_filter(
            None,
            execution,
            indexed_edge_equality,
            parameters,
            label_id.map(|id| id.raw()),
        )?;
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
                src_id,
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
                        store, parameters, dst, dst_id, dst_filter, caller, execution,
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
            stream_row_through_ops(at.next(), out, expanded, execution, evaluator)
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
        EdgeSequenceOrder::Ascending,
        indexed_edge_equality,
        edge_inline_property_predicate,
        edge_inline_vector_predicate,
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
                    store, parameters, dst, dst_id, dst_filter, caller, execution,
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
        if stream_row_through_ops(at.next(), out, expanded, execution, evaluator)? {
            return Ok(true);
        }
    }
    Ok(false)
}
