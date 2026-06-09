//! Plan operator dispatch (`execute_ops_from`).

use std::collections::BTreeSet;
use std::pin::Pin;

use gleaph_gql::Value;
use gleaph_gql_planner::plan::{InlineProcedureScope, PhysicalPlan, PlanOp};

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

use super::super::aggregate;
use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::context::ExecuteCtx;
use super::expand::{execute_expand, execute_var_len_expand};
use super::for_loop::execute_for;
use super::join::{execute_cartesian_product, execute_hash_join, merge_rows};
use super::path::execute_shortest_path;
use super::scan::{
    LIMITED_STREAMING_REMOTE_EXPAND_SOURCE, execute_conditional_index_scan,
    execute_edge_bind_endpoints, execute_edge_index_scan, execute_index_intersection,
    execute_index_scan, execute_limited_streaming_prefix, execute_node_scan,
    limited_streaming_prefix_limit_idx,
};
use super::set_operation::execute_set_operation;
use super::wcoj::execute_wcoj;
use super::{
    PlanBinding, binding_to_value, dedup_rows, ensure_simple_expand, ensure_var_len_expand,
    gleaph_sequence_order_after_expand, gleaph_sequence_sort, limit_value, plan_op_name,
    previous_op_binds_edge, project_row, row_matches_all, sort_rows,
};

pub(crate) async fn execute_ops(
    ctx: &ExecuteCtx<'_>,
    ops: &[PlanOp],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    execute_ops_from(ctx, ops, vec![PlanRow::new()]).await
}

/// Variables that operators in `ops` may bind (used to NULL-pad `OptionalMatch` miss rows).
///
/// Downstream mandatory [`Expand`] / [`ShortestPath`] ops skip rows whose traversal
/// endpoints are null-padded optional bindings instead of failing in [`vertex_binding`].
fn subplan_written_vars(ops: &[PlanOp]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for op in ops {
        extend_subplan_written_vars_from_op(op, &mut out);
    }
    out
}

fn extend_subplan_written_vars_from_op(op: &PlanOp, out: &mut BTreeSet<String>) {
    match op {
        PlanOp::NodeScan { variable, .. }
        | PlanOp::IndexScan { variable, .. }
        | PlanOp::EdgeIndexScan { variable, .. }
        | PlanOp::IndexIntersection { variable, .. } => {
            out.insert(variable.to_string());
        }
        PlanOp::ConditionalIndexScan {
            candidates,
            fallback_variable,
            ..
        } => {
            out.insert(fallback_variable.to_string());
            for c in candidates {
                out.insert(c.variable.to_string());
            }
        }
        PlanOp::EdgeBindEndpoints {
            edge,
            near,
            far,
            hop_aux_binding,
            ..
        } => {
            out.insert(edge.to_string());
            out.insert(near.to_string());
            out.insert(far.to_string());
            if let Some(h) = hop_aux_binding {
                out.insert(h.to_string());
            }
            // When EdgeBindEndpoints execution is implemented, `far` must honor
            // `expand_dst_matches_prebound_vertex` if `far` is already vertex-bound.
        }
        PlanOp::Expand {
            edge,
            dst,
            hop_aux_binding,
            var_len,
            path_var,
            ..
        }
        | PlanOp::ExpandFilter {
            edge,
            dst,
            hop_aux_binding,
            var_len,
            path_var,
            ..
        } => {
            out.insert(edge.to_string());
            out.insert(dst.to_string());
            if let Some(h) = hop_aux_binding {
                out.insert(h.to_string());
            }
            if var_len.is_some()
                && let Some(p) = path_var
            {
                out.insert(p.to_string());
            }
        }
        PlanOp::ShortestPath { edge, path_var, .. } => {
            out.insert(edge.to_string());
            if let Some(p) = path_var {
                out.insert(p.to_string());
            }
        }
        PlanOp::Let { bindings } => {
            for b in bindings {
                out.insert(b.variable.clone());
            }
        }
        PlanOp::For {
            variable,
            ordinality,
            ..
        } => {
            out.insert(variable.to_string());
            if let Some(o) = ordinality {
                out.insert(o.to_string());
            }
        }
        PlanOp::WorstCaseOptimalJoin { variables, edges } => {
            for v in variables {
                out.insert(v.to_string());
            }
            for e in edges {
                out.insert(e.variable.to_string());
                if let Some(h) = &e.hop_aux_binding {
                    out.insert(h.to_string());
                }
            }
        }
        PlanOp::OptionalMatch { sub_plan }
        | PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => {
            for child in sub_plan {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::UseGraph { sub_plan: None, .. } => {}
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            for child in left {
                extend_subplan_written_vars_from_op(child, out);
            }
            for child in right {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            for child in &sub_plan.ops {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::SetOperation { right, .. } => {
            for child in &right.ops {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::InsertVertex { variable, .. } => {
            if let Some(v) = variable {
                out.insert(v.to_string());
            }
        }
        PlanOp::InsertEdge { variable, .. } => {
            if let Some(v) = variable {
                out.insert(v.to_string());
            }
        }
        PlanOp::PropertyFilter { .. }
        | PlanOp::Filter { .. }
        | PlanOp::CallProcedure { .. }
        | PlanOp::Aggregate { .. }
        | PlanOp::Sort { .. }
        | PlanOp::Limit { .. }
        | PlanOp::TopK { .. }
        | PlanOp::SetProperties { .. }
        | PlanOp::RemoveProperties { .. }
        | PlanOp::DeleteVertex { .. }
        | PlanOp::DetachDeleteVertex { .. }
        | PlanOp::DeleteEdge { .. } => {}
        PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
            for col in columns {
                if let Some(alias) = &col.alias {
                    out.insert(alias.to_string());
                }
            }
        }
    }
}

fn ops_contain_set_operation(ops: &[PlanOp]) -> bool {
    ops.iter().any(op_contains_set_operation)
}

fn op_contains_set_operation(op: &PlanOp) -> bool {
    match op {
        PlanOp::SetOperation { .. } => true,
        PlanOp::OptionalMatch { sub_plan }
        | PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => ops_contain_set_operation(sub_plan),
        PlanOp::UseGraph { sub_plan: None, .. } => false,
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            ops_contain_set_operation(left) || ops_contain_set_operation(right)
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => ops_contain_set_operation(&sub_plan.ops),
        _ => false,
    }
}

async fn execute_optional_match(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    sub_plan: &[PlanOp],
    written: &BTreeSet<String>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut out = Vec::new();
    for row in rows {
        let extended = execute_ops_from(ctx, sub_plan, vec![row.clone()]).await?;
        if extended.is_empty() {
            let mut padded = row;
            for v in written {
                if !padded.contains_key(v) {
                    padded.insert(v.clone(), PlanBinding::Value(Value::Null));
                }
            }
            out.push(padded);
        } else {
            out.extend(extended);
        }
    }
    Ok(out)
}

fn inline_seed_row(row: &PlanRow, scope: &InlineProcedureScope) -> Result<PlanRow, PlanQueryError> {
    match scope {
        InlineProcedureScope::ImplicitAll => Ok(row.clone()),
        InlineProcedureScope::Explicit(vars) => {
            let mut seed = PlanRow::new();
            for v in vars {
                match row.get(v.as_ref()) {
                    Some(binding) => {
                        seed.insert(v.to_string(), binding.clone());
                    }
                    None => {
                        return Err(PlanQueryError::MissingBinding {
                            variable: v.to_string(),
                        });
                    }
                }
            }
            Ok(seed)
        }
    }
}

fn merge_inline_rows(
    store: &crate::facade::GraphStore,
    outer: &PlanRow,
    inner: PlanRow,
) -> Result<PlanRow, PlanQueryError> {
    if let Some(merged) = merge_rows(outer, &inner) {
        return Ok(merged);
    }

    let mut merged = outer.clone();
    for (name, inner_binding) in inner.iter() {
        match outer.get(name) {
            Some(outer_binding) if inline_bindings_match(store, outer_binding, inner_binding)? => {}
            Some(_) => {
                return Err(PlanQueryError::InvalidExpressionValue {
                    expression: "inline CALL: conflicting bindings on merge".into(),
                });
            }
            None => merged.insert(name.to_string(), inner_binding.clone()),
        }
    }
    Ok(merged)
}

fn inline_bindings_match(
    store: &crate::facade::GraphStore,
    left: &PlanBinding,
    right: &PlanBinding,
) -> Result<bool, PlanQueryError> {
    if left == right {
        return Ok(true);
    }
    Ok(binding_to_value(store, None, left)? == binding_to_value(store, None, right)?)
}

async fn execute_inline_procedure_call(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    sub_plan: &PhysicalPlan,
    scope: &InlineProcedureScope,
    optional: bool,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let optional_padding_vars = inline_output_vars(sub_plan);
    let mut out = Vec::new();
    for outer_row in rows {
        let seed = inline_seed_row(&outer_row, scope)?;
        let sub_rows = execute_ops_from(ctx, &sub_plan.ops, vec![seed]).await?;
        if sub_rows.is_empty() {
            if optional {
                let mut padded = outer_row;
                for v in &optional_padding_vars {
                    if !padded.contains_key(v) {
                        padded.insert(v.clone(), PlanBinding::Value(Value::Null));
                    }
                }
                out.push(padded);
            }
        } else {
            for inner in sub_rows {
                out.push(merge_inline_rows(ctx.store, &outer_row, inner)?);
            }
        }
    }
    Ok(out)
}

fn inline_output_vars(sub_plan: &PhysicalPlan) -> BTreeSet<String> {
    if !sub_plan.output.hydrates_all_row_bindings() {
        return sub_plan
            .output
            .columns
            .iter()
            .map(|col| col.name.to_string())
            .collect();
    }
    subplan_written_vars(&sub_plan.ops)
}

fn limited_streaming_prefix_expand_count(ops: &[PlanOp]) -> usize {
    ops.iter()
        .filter(|op| matches!(op, PlanOp::Expand { .. } | PlanOp::ExpandFilter { .. }))
        .count()
}

fn limited_streaming_prefix_has_remote_expand_source(ops: &[PlanOp], rows: &[PlanRow]) -> bool {
    ops.iter().any(|op| {
        let src = match op {
            PlanOp::Expand { src, .. } | PlanOp::ExpandFilter { src, .. } => src,
            _ => return false,
        };
        rows.iter()
            .any(|row| matches!(row.get(src.as_ref()), Some(PlanBinding::RemoteVertex(_))))
    })
}

pub(crate) fn execute_ops_from<'a>(
    ctx: &'a ExecuteCtx<'a>,
    ops: &'a [PlanOp],
    initial_rows: Vec<PlanRow>,
) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<PlanRow>, PlanQueryError>> + 'a>> {
    Box::pin(async move {
        let store = ctx.store;
        let parameters = ctx.parameters;
        let _caller = ctx.caller();
        let gwd = ctx.gleaph_weight_decoders;
        let set_operation_input = ops_contain_set_operation(ops).then(|| initial_rows.clone());
        let mut rows = initial_rows;
        // Index of the nearest preceding `PlanOp::Aggregate` for resolving
        // `ExprKind::Aggregate` in post-aggregate ops (e.g. `HAVING`).
        let mut active_aggregate_op_idx: Option<usize> = None;

        let mut op_idx = 0;
        while op_idx < ops.len() {
            let op = &ops[op_idx];
            let aggregate_specs = active_aggregate_op_idx.and_then(|idx| match &ops[idx] {
                PlanOp::Aggregate { aggregates, .. } => Some(aggregates.as_slice()),
                _ => None,
            });
            let evaluator = ctx.expr_evaluator(aggregate_specs);
            if let Some(limit_idx) = limited_streaming_prefix_limit_idx(ops, op_idx) {
                let prefix_ops = &ops[op_idx..=limit_idx];
                let expand_count = limited_streaming_prefix_expand_count(prefix_ops);
                if expand_count > 0
                    && limited_streaming_prefix_has_remote_expand_source(prefix_ops, &rows)
                {
                    // Remote expand sources need async placement/federated routing; execute the
                    // regular async operator path below.
                } else if expand_count > 1 {
                    // A later expand in the same prefix may see a remote vertex emitted by an
                    // earlier expand, so keep the original rows available for async fallback.
                    let streaming_input = rows;
                    let result = execute_limited_streaming_prefix(
                        ctx.store,
                        prefix_ops,
                        streaming_input.clone(),
                        ctx.parameters,
                        ctx.caller(),
                        &ctx.execution,
                        ctx.gleaph_weight_decoders,
                        aggregate_specs,
                    );
                    match result {
                        Ok(result) => {
                            rows = result.rows;
                            if result.clears_active_aggregate {
                                active_aggregate_op_idx = None;
                            }
                            op_idx = limit_idx + 1;
                            continue;
                        }
                        Err(PlanQueryError::UnsupportedOp(op))
                            if op == LIMITED_STREAMING_REMOTE_EXPAND_SOURCE =>
                        {
                            rows = streaming_input;
                        }
                        Err(err) => return Err(err),
                    }
                } else {
                    let result = execute_limited_streaming_prefix(
                        ctx.store,
                        prefix_ops,
                        rows,
                        ctx.parameters,
                        ctx.caller(),
                        &ctx.execution,
                        ctx.gleaph_weight_decoders,
                        aggregate_specs,
                    )?;
                    rows = result.rows;
                    if result.clears_active_aggregate {
                        active_aggregate_op_idx = None;
                    }
                    op_idx = limit_idx + 1;
                    continue;
                }
            }
            rows = match op {
                PlanOp::NodeScan {
                    variable,
                    label,
                    property_projection: _,
                } => execute_node_scan(store, rows, variable, label.as_deref(), &ctx.execution)?,
                PlanOp::IndexScan {
                    variable,
                    property,
                    value,
                    cmp,
                    property_projection: _,
                } => {
                    execute_index_scan(ctx, rows, variable.as_ref(), property.as_ref(), value, *cmp)
                        .await?
                }
                PlanOp::ConditionalIndexScan {
                    candidates,
                    fallback_label,
                    fallback_variable,
                    property_projection: _,
                } => {
                    execute_conditional_index_scan(
                        ctx,
                        rows,
                        candidates,
                        fallback_label.as_deref(),
                        fallback_variable,
                    )
                    .await?
                }
                PlanOp::IndexIntersection {
                    variable,
                    scans,
                    property_projection: _,
                } => execute_index_intersection(&ctx, rows, variable.as_ref(), scans).await?,
                PlanOp::EdgeIndexScan {
                    variable,
                    property,
                    value,
                    property_projection: _,
                } => execute_edge_index_scan(store, rows, variable, property, value, parameters)?,
                PlanOp::EdgeBindEndpoints {
                    edge,
                    near,
                    far,
                    direction,
                    label,
                    near_property_projection,
                    far_property_projection,
                    hop_aux_binding,
                } => execute_edge_bind_endpoints(
                    store,
                    &ctx.execution,
                    rows,
                    edge,
                    near,
                    far,
                    *direction,
                    label.as_deref(),
                    near_property_projection.as_deref(),
                    far_property_projection.as_deref(),
                    hop_aux_binding.as_ref(),
                )?,
                PlanOp::PropertyFilter { predicates, .. } => rows
                    .into_iter()
                    .filter_map(|row| match row_matches_all(&evaluator, &row, predicates) {
                        Ok(true) => Some(Ok(row)),
                        Ok(false) => None,
                        Err(err) => Some(Err(err)),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                PlanOp::Let { bindings } => rows
                    .into_iter()
                    .map(|mut row| -> Result<PlanRow, PlanQueryError> {
                        for binding in bindings {
                            let value = evaluator.eval_expr(&row, &binding.value)?;
                            row.insert(binding.variable.clone(), PlanBinding::Value(value));
                        }
                        Ok(row)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                PlanOp::For {
                    variable,
                    list,
                    ordinality,
                    offset_keyword,
                } => execute_for(
                    &evaluator,
                    rows,
                    variable.as_ref(),
                    list,
                    ordinality.as_deref(),
                    *offset_keyword,
                )?,
                PlanOp::Filter { condition } => rows
                    .into_iter()
                    .filter_map(|row| {
                        match row_matches_all(&evaluator, &row, std::slice::from_ref(condition)) {
                            Ok(true) => Some(Ok(row)),
                            Ok(false) => None,
                            Err(err) => Some(Err(err)),
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?,
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
                        ensure_var_len_expand(
                            label_expr,
                            hop_aux_binding,
                            indexed_edge_equality,
                            edge_payload_predicate,
                            edge_vector_predicate,
                            edge_property_projection,
                        )?;
                        execute_var_len_expand(
                            ctx,
                            rows,
                            src,
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.as_ref(),
                            &ctx.execution,
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
                        )
                        .await?
                    } else {
                        ensure_simple_expand(label_expr, var_len, hop_aux_binding)?;
                        let sequence_order = gleaph_sequence_order_after_expand(
                            ops,
                            op_idx,
                            edge.as_ref(),
                            label.is_some() && label_expr.is_none(),
                        )?;
                        execute_expand(
                            ctx,
                            rows,
                            src,
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.as_ref(),
                            &ctx.execution,
                            sequence_order,
                            &[],
                            *emit_edge_binding,
                            hop_aux_binding.as_ref(),
                            indexed_edge_equality.as_ref(),
                            edge_payload_predicate.as_ref(),
                            edge_vector_predicate.as_ref(),
                            edge_property_projection.as_deref(),
                            dst_property_projection.as_deref(),
                        )
                        .await?
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
                        ensure_var_len_expand(
                            label_expr,
                            hop_aux_binding,
                            indexed_edge_equality,
                            edge_payload_predicate,
                            edge_vector_predicate,
                            edge_property_projection,
                        )?;
                        execute_var_len_expand(
                            ctx,
                            rows,
                            src,
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.as_ref(),
                            &ctx.execution,
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
                        )
                        .await?
                    } else {
                        ensure_simple_expand(label_expr, var_len, hop_aux_binding)?;
                        let sequence_order = gleaph_sequence_order_after_expand(
                            ops,
                            op_idx,
                            edge.as_ref(),
                            label.is_some() && label_expr.is_none(),
                        )?;
                        execute_expand(
                            ctx,
                            rows,
                            src,
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.as_ref(),
                            &ctx.execution,
                            sequence_order,
                            dst_filter,
                            *emit_edge_binding,
                            hop_aux_binding.as_ref(),
                            indexed_edge_equality.as_ref(),
                            edge_payload_predicate.as_ref(),
                            edge_vector_predicate.as_ref(),
                            edge_property_projection.as_deref(),
                            dst_property_projection.as_deref(),
                        )
                        .await?
                    }
                }
                PlanOp::ShortestPath {
                    src,
                    dst,
                    edge,
                    path_var,
                    emit_edge_binding,
                    emit_path_binding,
                    mode,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    cost,
                } => {
                    execute_shortest_path(
                        store,
                        rows,
                        src,
                        dst,
                        edge,
                        path_var.as_ref(),
                        *emit_edge_binding,
                        *emit_path_binding,
                        *mode,
                        *direction,
                        label.as_deref(),
                        &ctx.execution,
                        label_expr,
                        var_len,
                        cost,
                        parameters,
                        gwd,
                        &ops[op_idx + 1..],
                    )
                    .await?
                }
                PlanOp::Aggregate {
                    group_by,
                    aggregates,
                } => {
                    let agg_evaluator = ctx.expr_evaluator(None);
                    let out =
                        aggregate::execute_aggregate(rows, group_by, aggregates, &agg_evaluator)?;
                    active_aggregate_op_idx = Some(op_idx);
                    out
                }
                PlanOp::Project { columns, distinct } => {
                    #[cfg(all(feature = "canbench", target_family = "wasm"))]
                    let _scope = bench_scope("plan_op_project");
                    let proj_evaluator = ctx.expr_evaluator(aggregate_specs);
                    let mut projected = rows
                        .iter()
                        .map(|row| project_row(&proj_evaluator, row, columns))
                        .collect::<Result<Vec<_>, _>>()?;
                    if *distinct {
                        dedup_rows(&mut projected);
                    }
                    active_aggregate_op_idx = None;
                    projected
                }
                PlanOp::Limit { count, offset } => {
                    let offset = match offset {
                        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
                        None => 0,
                    };
                    let count = match count {
                        Some(expr) => {
                            Some(limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?)
                        }
                        None => None,
                    };
                    rows.into_iter()
                        .skip(offset)
                        .take(count.unwrap_or(usize::MAX))
                        .collect()
                }
                PlanOp::Sort { order_by }
                    if gleaph_sequence_sort(order_by).is_some_and(|(edge_var, _)| {
                        previous_op_binds_edge(ops, op_idx, edge_var.as_str())
                    }) =>
                {
                    rows
                }
                PlanOp::Sort { order_by } => sort_rows(&evaluator, rows, order_by)?,
                PlanOp::TopK {
                    order_by,
                    k,
                    offset,
                } if gleaph_sequence_sort(order_by).is_some_and(|(edge_var, _)| {
                    previous_op_binds_edge(ops, op_idx, edge_var.as_str())
                }) =>
                {
                    let offset = match offset {
                        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
                        None => 0,
                    };
                    let k = limit_value(&evaluator.eval_expr(&PlanRow::new(), k)?)?;
                    rows.into_iter().skip(offset).take(k).collect()
                }
                PlanOp::TopK {
                    order_by,
                    k,
                    offset,
                } => {
                    let offset = match offset {
                        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
                        None => 0,
                    };
                    let k = limit_value(&evaluator.eval_expr(&PlanRow::new(), k)?)?;
                    sort_rows(&evaluator, rows, order_by)?
                        .into_iter()
                        .skip(offset)
                        .take(k)
                        .collect()
                }
                PlanOp::Materialize { columns, distinct } => {
                    let mut materialized = rows
                        .iter()
                        .map(|row| project_row(&evaluator, row, columns))
                        .collect::<Result<Vec<_>, _>>()?;
                    if *distinct {
                        dedup_rows(&mut materialized);
                    }
                    materialized
                }
                PlanOp::UseGraph {
                    graph_name: _,
                    sub_plan: Some(sub_plan),
                } => {
                    // v1 has a single physical GraphStore; USE scopes its sub-plan
                    // but does not route to a separate graph store yet.
                    execute_ops_from(ctx, sub_plan, rows).await?
                }
                PlanOp::UseGraph {
                    graph_name: _,
                    sub_plan: None,
                } => {
                    // Same single-store v1 behavior: a bare USE marker is metadata.
                    rows
                }
                PlanOp::CartesianProduct { left, right } => {
                    execute_cartesian_product(ctx, rows, left, right).await?
                }
                PlanOp::HashJoin {
                    left,
                    right,
                    join_keys,
                } => execute_hash_join(ctx, rows, left, right, join_keys).await?,
                PlanOp::WorstCaseOptimalJoin { variables, edges } => {
                    execute_wcoj(ctx, rows, variables, edges).await?
                }
                PlanOp::OptionalMatch { sub_plan } => {
                    let written = subplan_written_vars(sub_plan);
                    execute_optional_match(ctx, rows, sub_plan, &written).await?
                }
                PlanOp::InlineProcedureCall {
                    sub_plan,
                    scope,
                    optional,
                } => execute_inline_procedure_call(ctx, rows, sub_plan, scope, *optional).await?,
                PlanOp::SetOperation { op, right } => {
                    let right_input = set_operation_input
                        .clone()
                        .expect("set operation input must exist when executing SetOperation");
                    execute_set_operation(ctx, rows, *op, right, right_input).await?
                }
                other if other.is_dml() => {
                    return Err(PlanQueryError::UnsupportedOp(plan_op_name(other)));
                }
                other => return Err(PlanQueryError::UnsupportedOp(plan_op_name(other))),
            };
            op_idx += 1;
        }

        Ok(rows)
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;

    #[test]
    fn executes_planner_use_graph_as_single_store_pass_through() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryUseGraph"],
                [("name", Value::Text("Planner UseGraph".into()))],
            )
            .expect("insert vertex");
        let plan = plan_gql("USE myGraph MATCH (n:PlannerQueryUseGraph) RETURN n.name AS name");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner UseGraph".into()))
        );
    }

    #[test]
    fn executes_planner_cartesian_product_for_independent_matches() {
        let store = GraphStore::new();
        for name in ["Planner CP Alice", "Planner CP Bob"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryCartesianPerson"],
                    [("name", Value::Text(name.into()))],
                )
                .expect("insert person");
        }
        for city in ["Planner CP Tokyo", "Planner CP Paris"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryCartesianCity"],
                    [("name", Value::Text(city.into()))],
                )
                .expect("insert city");
        }
        let plan = plan_gql(
            "MATCH (a:PlannerQueryCartesianPerson) MATCH (b:PlannerQueryCartesianCity) \
             RETURN a.name AS person, b.name AS city",
        );
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::CartesianProduct { .. }))
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 4);
        assert!(result.rows.iter().any(|row| {
            row.get("person") == Some(&Value::Text("Planner CP Alice".into()))
                && row.get("city") == Some(&Value::Text("Planner CP Tokyo".into()))
        }));
        assert!(result.rows.iter().any(|row| {
            row.get("person") == Some(&Value::Text("Planner CP Bob".into()))
                && row.get("city") == Some(&Value::Text("Planner CP Paris".into()))
        }));
    }

    #[test]
    fn optional_match_planner_null_padding_when_no_edge() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptMatchA"], [("name", Value::Text("solo".into()))])
            .expect("insert vertex");
        let gql = "MATCH (n:OptMatchA) OPTIONAL MATCH (n)-[e:OptMatchRel]->(m:OptMatchB) \
                   RETURN n.name AS nn, m.name AS mn";
        let plan = plan_gql(gql);
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::OptionalMatch { .. })),
            "expected OptionalMatch in plan: {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute optional match");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("nn"), Some(&Value::Text("solo".into())));
        assert_eq!(result.rows[0].get("mn"), Some(&Value::Null));
    }

    #[test]
    fn optional_match_planner_returns_m_when_edge_exists() {
        let store = GraphStore::new();
        let n = store
            .insert_vertex_named(["OptMatchA2"], [("name", Value::Text("a".into()))])
            .expect("insert n");
        let m = store
            .insert_vertex_named(["OptMatchB2"], [("name", Value::Text("buddy".into()))])
            .expect("insert m");
        store
            .insert_directed_edge_named(n, m, Some("OptMatchRel2"), Vec::<(&str, Value)>::new())
            .expect("insert edge");
        let gql = "MATCH (n:OptMatchA2) OPTIONAL MATCH (n)-[e:OptMatchRel2]->(m:OptMatchB2) \
                   RETURN m.name AS mn";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute optional match");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("mn"), Some(&Value::Text("buddy".into())));
    }

    #[test]
    fn optional_match_leading_empty_graph_null_binds_pattern_var() {
        let store = GraphStore::new();
        let gql = "OPTIONAL MATCH (n:OptMatchLeading) RETURN n IS NULL AS is_n_null";
        let plan = plan_gql(gql);
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::OptionalMatch { .. })),
            "expected OptionalMatch: {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute leading optional");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("is_n_null"), Some(&Value::Bool(true)));
    }

    #[test]
    fn mandatory_match_after_optional_miss_drops_null_bound_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptChainA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        store
            .insert_vertex_named(["OptChainB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        crate::test_labels::edge_label_id_for_name("OptChainRel");
        let gql = "MATCH (a:OptChainA) OPTIONAL MATCH (a)-[e:OptChainRel]->(b:OptChainB) \
                   MATCH (b)-[e2:OptChainRel]->(c:OptChainB) RETURN a, b, c";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("mandatory match after optional miss should not error");
        assert!(
            result.rows.is_empty(),
            "optional miss leaves b null; mandatory follow-on match should drop the row: {:?}",
            result.rows
        );
    }

    #[test]
    fn mandatory_match_after_optional_hit_continues_chain() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["OptChainA2"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let b = store
            .insert_vertex_named(["OptChainB2"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        let c = store
            .insert_vertex_named(["OptChainC2"], Vec::<(&str, Value)>::new())
            .expect("insert c");
        store
            .insert_directed_edge_named(a, b, Some("OptChainRel2"), Vec::<(&str, Value)>::new())
            .expect("a->b");
        store
            .insert_directed_edge_named(b, c, Some("OptChainRel2"), Vec::<(&str, Value)>::new())
            .expect("b->c");
        let gql = "MATCH (a:OptChainA2) OPTIONAL MATCH (a)-[e:OptChainRel2]->(b:OptChainB2) \
                   MATCH (b)-[e2:OptChainRel2]->(c:OptChainC2) RETURN a, b, c";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("mandatory match after optional hit");
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn rebound_node_label_is_enforced_without_rescan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["RebindA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let gql = "MATCH (a:RebindA) MATCH (a:RebindB) RETURN a";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("rebound label check");
        assert!(
            result.rows.is_empty(),
            "vertex labeled RebindA must not satisfy rebound RebindB match: {:?}",
            result.rows
        );
    }

    #[test]
    fn rebound_label_succeeds_when_vertex_has_both_labels() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["DualA", "DualB"], Vec::<(&str, Value)>::new())
            .expect("insert dual-label vertex");
        let gql = "MATCH (a:DualA) MATCH (a:DualB) RETURN a";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("dual-label rebound");
        assert_eq!(
            result.rows.len(),
            1,
            "vertex with both labels must satisfy sequential label matches: {:?}",
            result.rows
        );
    }

    // Manual NodeScan + PropertyFilter plans: `plan_gql` may emit IndexScan for inline
    // label properties, which fails in tests without an index client.    #[test]
    fn rebound_inline_property_fails_when_value_mismatches() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PropRebindA"], [("nick", Value::Text("x".into()))])
            .expect("insert a");
        let nick_eq = |value: &str| {
            Expr::new(ExprKind::Compare {
                left: Box::new(Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::var("a")),
                    property: "nick".into(),
                })),
                op: gleaph_gql::ast::CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Text(value.into())))),
            })
        };
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("PropRebindA".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![nick_eq("x")],
                stage: 0,
            },
            PlanOp::PropertyFilter {
                predicates: vec![nick_eq("y")],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![project(var("a"), "a")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("rebound inline property mismatch");
        assert!(
            result.rows.is_empty(),
            "stricter rebound property must filter mismatched rows: {:?}",
            result.rows
        );
    }

    #[test]
    fn rebound_inline_property_succeeds_when_value_matches() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PropRebindB"], [("nick", Value::Text("same".into()))])
            .expect("insert a");
        let nick_eq = Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::var("a")),
                property: "nick".into(),
            })),
            op: gleaph_gql::ast::CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Text("same".into())))),
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("PropRebindB".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![nick_eq.clone()],
                stage: 0,
            },
            PlanOp::PropertyFilter {
                predicates: vec![nick_eq],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![project(var("a"), "a")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("rebound inline property match");
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn optional_match_manual_null_padding_edge_and_dst() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptManualN"], Vec::<(&str, Value)>::new())
            .expect("insert n");
        let expand = PlanOp::Expand {
            src: "n".into(),
            edge: "e".into(),
            dst: "m".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("OptManualRel".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        };
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("OptManualN".into()),
                property_projection: None,
            },
            PlanOp::OptionalMatch {
                sub_plan: vec![expand],
            },
            PlanOp::Project {
                columns: vec![
                    project(Expr::new(ExprKind::IsNull(Box::new(var("e")))), "e_null"),
                    project(Expr::new(ExprKind::IsNull(Box::new(var("m")))), "m_null"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute manual optional");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("e_null"), Some(&Value::Bool(true)));
        assert_eq!(result.rows[0].get("m_null"), Some(&Value::Bool(true)));
    }

    #[test]
    fn optional_match_gleaph_weight_on_null_edge_returns_null() {
        let store = GraphStore::new();
        crate::test_labels::edge_label_id_for_name("NullWgtRel");
        store
            .install_edge_label_weight_profile_at_init(
                crate::test_labels::edge_label_id_for_name("NullWgtRel"),
                gleaph_graph_kernel::entry::EdgeWeightProfile {
                    encoding: gleaph_graph_kernel::entry::WeightEncoding::RawU16,
                },
            )
            .expect("profile");
        store
            .insert_vertex_named(["NullWgtN"], Vec::<(&str, Value)>::new())
            .expect("insert n");
        let gql = "MATCH (n:NullWgtN) OPTIONAL MATCH (n)-[e:NullWgtRel]->(m) \
                   RETURN GLEAPH.WEIGHT(e) AS w";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("gleaph weight on optional miss should return null");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("w"), Some(&Value::Null));
    }

    #[test]
    fn executes_union_all_composite_query() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["SetOpUnionA"], [("name", Value::Text("alpha".into()))])
            .expect("insert a");
        store
            .insert_vertex_named(["SetOpUnionB"], [("name", Value::Text("beta".into()))])
            .expect("insert b");
        let plan = plan_statement_gql(
            "MATCH (n:SetOpUnionA) RETURN n.name AS name \
             UNION ALL \
             MATCH (m:SetOpUnionB) RETURN m.name AS name",
        );
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::SetOperation { .. })),
            "expected SetOperation: {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("union all");
        assert_eq!(result.rows.len(), 2);
        let names: Vec<_> = result
            .rows
            .iter()
            .filter_map(|r| r.get("name"))
            .cloned()
            .collect();
        assert!(names.contains(&Value::Text("alpha".into())));
        assert!(names.contains(&Value::Text("beta".into())));
    }

    #[test]
    fn executes_union_distinct_dedups_matching_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["SetOpDistinct"], [("k", Value::Int64(1))])
            .expect("insert");
        let plan = plan_statement_gql(
            "MATCH (n:SetOpDistinct) RETURN n.k AS k \
             UNION \
             MATCH (m:SetOpDistinct) RETURN m.k AS k",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("union distinct");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("k"), Some(&Value::Int64(1)));
    }

    #[test]
    fn executes_union_distinct_dedups_projected_vertex_rows() {
        let store = GraphStore::new();
        let vertex = store
            .insert_vertex_named(["SetOpVertexDistinct"], [("k", Value::Int64(1))])
            .expect("insert");
        let plan = plan_statement_gql(
            "MATCH (n:SetOpVertexDistinct) RETURN n \
             UNION \
             MATCH (m:SetOpVertexDistinct) RETURN m AS n",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("union distinct");
        assert_eq!(result.rows.len(), 1);
        let Value::Record(record) = result.rows[0].get("n").expect("n") else {
            panic!("expected vertex record");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value)),
            Some(&Value::Uint64(u64::from(vertex)))
        );
    }

    #[test]
    fn executes_except_removes_right_branch_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["SetOpExceptL"], [("k", Value::Int64(1))])
            .expect("left");
        store
            .insert_vertex_named(["SetOpExceptR"], [("k", Value::Int64(2))])
            .expect("right");
        let plan = plan_statement_gql(
            "MATCH (n:SetOpExceptL) RETURN n.k AS k \
             EXCEPT \
             MATCH (m:SetOpExceptR) RETURN m.k AS k",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("except");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("k"), Some(&Value::Int64(1)));
    }

    #[test]
    fn executes_except_distinct_dedups_left_branch_rows() {
        let store = GraphStore::new();
        for _ in 0..2 {
            store
                .insert_vertex_named(["SetOpExceptDup"], [("k", Value::Int64(1))])
                .expect("left");
        }
        let plan = plan_statement_gql(
            "MATCH (n:SetOpExceptDup) RETURN n.k AS k \
             EXCEPT \
             MATCH (m:SetOpExceptMissing) RETURN m.k AS k",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("except distinct");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("k"), Some(&Value::Int64(1)));
    }

    #[test]
    fn executes_intersect_all_preserves_multiplicity() {
        let store = GraphStore::new();
        for _ in 0..2 {
            store
                .insert_vertex_named(["SetOpIntersect"], [("k", Value::Int64(7))])
                .expect("insert");
        }
        let plan = plan_statement_gql(
            "MATCH (n:SetOpIntersect) RETURN n.k AS k \
             INTERSECT ALL \
             MATCH (m:SetOpIntersect) RETURN m.k AS k",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("intersect all");
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn executes_otherwise_returns_left_when_non_empty() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["OtherwiseLeft"],
                [("name", Value::Text("left-only".into()))],
            )
            .expect("insert left");
        store
            .insert_vertex_named(
                ["OtherwiseRight"],
                [("name", Value::Text("right-fallback".into()))],
            )
            .expect("insert right");
        let plan = plan_statement_gql(
            "MATCH (n:OtherwiseLeft) RETURN n.name AS name \
             OTHERWISE \
             MATCH (m:OtherwiseRight) RETURN m.name AS name",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("otherwise non-empty left");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("left-only".into()))
        );
    }

    #[test]
    fn executes_otherwise_falls_back_when_left_empty() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["OtherwiseRightOnly"],
                [("name", Value::Text("fallback".into()))],
            )
            .expect("insert right");
        let plan = plan_statement_gql(
            "MATCH (n:OtherwiseMissing) RETURN n.name AS name \
             OTHERWISE \
             MATCH (m:OtherwiseRightOnly) RETURN m.name AS name",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("otherwise empty left");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("fallback".into()))
        );
    }

    #[test]
    fn executes_chained_otherwise_reaches_third_branch() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["OtherwisePresentC"],
                [("name", Value::Text("third".into()))],
            )
            .expect("insert third");
        let plan = plan_statement_gql(
            "MATCH (n:OtherwiseMissingA) RETURN n.name AS name \
             OTHERWISE \
             MATCH (m:OtherwiseMissingB) RETURN m.name AS name \
             OTHERWISE \
             MATCH (p:OtherwisePresentC) RETURN p.name AS name",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("chained otherwise");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("third".into()))
        );
    }

    #[test]
    fn executes_for_literal_list() {
        let store = GraphStore::new();
        let plan = plan_gql("FOR x IN [1, 2, 3] RETURN x");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("for literal");
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0].get("x"), Some(&Value::Int64(1)));
        assert_eq!(result.rows[2].get("x"), Some(&Value::Int64(3)));
    }

    #[test]
    fn executes_for_with_ordinality() {
        let store = GraphStore::new();
        let plan = plan_gql("FOR x IN [10, 20] WITH ORDINALITY i RETURN x, i");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("for ordinality");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].get("i"), Some(&Value::Int64(1)));
        assert_eq!(result.rows[1].get("i"), Some(&Value::Int64(2)));
    }

    #[test]
    fn executes_for_with_offset() {
        let store = GraphStore::new();
        let plan = plan_gql("FOR x IN [10, 20] WITH OFFSET o RETURN x, o");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("for offset");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].get("o"), Some(&Value::Int64(0)));
        assert_eq!(result.rows[1].get("o"), Some(&Value::Int64(1)));
    }

    #[test]
    fn executes_for_empty_list_returns_no_rows() {
        let store = GraphStore::new();
        let plan = plan_gql("FOR x IN [] RETURN x");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("for empty");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn executes_for_after_match_expands_list_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["ForTagsNode"],
                [(
                    "tags",
                    Value::List(vec![Value::Text("x".into()), Value::Text("y".into())]),
                )],
            )
            .expect("insert");
        let plan = plan_gql("MATCH (n:ForTagsNode) FOR t IN n.tags RETURN t");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("for after match");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].get("t"), Some(&Value::Text("x".into())));
        assert_eq!(result.rows[1].get("t"), Some(&Value::Text("y".into())));
    }

    #[test]
    fn inline_basic_correlated_call_returns_projected_binding() {
        let store = GraphStore::new();
        let vertex = store
            .insert_vertex_named(["InlineBasic"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql("MATCH (n:InlineBasic) CALL { RETURN n AS x } RETURN x");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline basic");
        assert_eq!(result.rows.len(), 1);
        let Value::Record(record) = result.rows[0].get("x").expect("x") else {
            panic!("expected vertex record for x");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value)),
            Some(&Value::Uint64(u64::from(vertex)))
        );
    }

    #[test]
    fn inline_call_expands_row_cardinality() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["InlineFor"], [("name", Value::Text("for-node".into()))])
            .expect("insert");
        let plan = plan_gql(
            "MATCH (n:InlineFor) CALL { FOR x IN [1, 2] RETURN x } RETURN n.name AS name, x",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline for");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("for-node".into()))
        );
        assert_eq!(result.rows[1].get("name"), result.rows[0].get("name"));
        assert_eq!(result.rows[0].get("x"), Some(&Value::Int64(1)));
        assert_eq!(result.rows[1].get("x"), Some(&Value::Int64(2)));
    }

    #[test]
    fn inline_call_filters_outer_row_when_subquery_empty() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["InlineEmpty"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql(
            "MATCH (n:InlineEmpty) CALL { MATCH (m:DefinitelyMissing) RETURN m } RETURN n",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline empty subquery");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn inline_scoped_call_reads_listed_variable() {
        let store = GraphStore::new();
        let vertex = store
            .insert_vertex_named(["InlineScope"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql("MATCH (n:InlineScope) CALL (n) { RETURN n AS x } RETURN x");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline scoped");
        assert_eq!(result.rows.len(), 1);
        let Value::Record(record) = result.rows[0].get("x").expect("x") else {
            panic!("expected vertex record for x");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value)),
            Some(&Value::Uint64(u64::from(vertex)))
        );
    }

    #[test]
    fn inline_explicit_empty_scope_runs_independent_subquery() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["InlineEmptyScope"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql("MATCH (n:InlineEmptyScope) CALL () { RETURN 1 AS x } RETURN n, x");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline explicit empty scope");
        assert_eq!(result.rows.len(), 1);
        assert!(result.rows[0].contains_key("n"));
        assert_eq!(result.rows[0].get("x"), Some(&Value::Int64(1)));
    }

    #[test]
    fn inline_return_star_preserves_outer_bindings() {
        let store = GraphStore::new();
        let vertex = store
            .insert_vertex_named(["InlineReturnStar"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql("MATCH (n:InlineReturnStar) CALL { RETURN * } RETURN n");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline return star");
        assert_eq!(result.rows.len(), 1);
        let Value::Record(record) = result.rows[0].get("n").expect("n") else {
            panic!("expected vertex record for n");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value)),
            Some(&Value::Uint64(u64::from(vertex)))
        );
    }

    #[test]
    fn inline_scoped_return_star_preserves_outer_bindings() {
        let store = GraphStore::new();
        let vertex = store
            .insert_vertex_named(["InlineScopedReturnStar"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql("MATCH (n:InlineScopedReturnStar) CALL (n) { RETURN * } RETURN n");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline scoped return star");
        assert_eq!(result.rows.len(), 1);
        let Value::Record(record) = result.rows[0].get("n").expect("n") else {
            panic!("expected vertex record for n");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value)),
            Some(&Value::Uint64(u64::from(vertex)))
        );
    }

    #[test]
    fn inline_composite_union_reads_outer_binding() {
        let store = GraphStore::new();
        let vertex = store
            .insert_vertex_named(["InlineUnion"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan =
            plan_gql("MATCH (n:InlineUnion) CALL { RETURN n AS x UNION RETURN n AS x } RETURN x");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline union");
        assert_eq!(result.rows.len(), 1);
        let Value::Record(record) = result.rows[0].get("x").expect("x") else {
            panic!("expected vertex record for x");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value)),
            Some(&Value::Uint64(u64::from(vertex)))
        );
    }

    #[test]
    fn inline_otherwise_falls_back_to_outer_binding() {
        let store = GraphStore::new();
        let vertex = store
            .insert_vertex_named(["InlineOtherwise"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql(
            "MATCH (n:InlineOtherwise) CALL { \
             MATCH (m:MissingInlineOtherwise) RETURN m AS x \
             OTHERWISE \
             RETURN n AS x \
             } RETURN x",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline otherwise");
        assert_eq!(result.rows.len(), 1);
        let Value::Record(record) = result.rows[0].get("x").expect("x") else {
            panic!("expected vertex record for x");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value)),
            Some(&Value::Uint64(u64::from(vertex)))
        );
    }

    #[test]
    fn inline_optional_call_null_pads_subquery_bindings() {
        let store = GraphStore::new();
        let vertex = store
            .insert_vertex_named(["InlineOptional"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql(
            "MATCH (n:InlineOptional) OPTIONAL CALL { \
             MATCH (m:MissingOptionalInline) RETURN m AS x \
             } RETURN n, x",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline optional");
        assert_eq!(result.rows.len(), 1);
        let Value::Record(record) = result.rows[0].get("n").expect("n") else {
            panic!("expected vertex record for n");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value)),
            Some(&Value::Uint64(u64::from(vertex)))
        );
        assert_eq!(result.rows[0].get("x"), Some(&Value::Null));
    }

    #[test]
    fn inline_optional_call_does_not_export_internal_match_bindings() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["InlineOptionalNoLeak"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql(
            "MATCH (n:InlineOptionalNoLeak) OPTIONAL CALL { \
             MATCH (m:MissingOptionalInlineNoLeak) RETURN m AS x \
             } RETURN *",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("inline optional no leak");
        assert_eq!(result.rows.len(), 1);
        assert!(result.rows[0].contains_key("n"));
        assert_eq!(result.rows[0].get("x"), Some(&Value::Null));
        assert!(
            !result.rows[0].contains_key("m"),
            "internal MATCH variable must not leak from OPTIONAL CALL miss: {:?}",
            result.rows[0]
        );
    }

    #[test]
    fn unsupported_operator_returns_stable_error() {
        let store = GraphStore::new();
        let cases = vec![(
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: Vec::new(),
                yield_columns: None,
                optional: false,
            },
            "CallProcedure",
        )];

        for (op, expected_name) in cases {
            let plan = plan(vec![op]);
            let err = store
                .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
                .expect_err("operator should be unsupported in v1");

            assert!(
                matches!(err, PlanQueryError::UnsupportedOp(name) if name == expected_name),
                "expected UnsupportedOp({expected_name}), got {err:?}"
            );
        }
    }
}
