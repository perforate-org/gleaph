//! Worst-case optimal join for simple cyclic patterns (e.g. triangles).

use gleaph_gql_planner::plan::{Str, WcojEdge};
use gleaph_graph_kernel::entry::EdgeLabelId;
use ic_stable_lara::VertexId;

use super::bindings::{EdgeBinding, hop_aux_scalar};
use super::context::ExecuteCtx;
use super::expand::{
    ExpandDst, edge_binding_matches_label_expr, expand_candidates_for_expand_op_into,
    expand_dst_binding, expand_dst_matches_prebound_vertex,
};
use super::{EdgeSequenceOrder, PlanBinding, row_matches_all};
use crate::gql_execution_context::GqlExecutionContext;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::row::PlanRow;

fn ensure_wcoj_supported(edges: &[WcojEdge]) -> Result<(), PlanQueryError> {
    for edge in edges {
        if edge.var_len.is_some() {
            return Err(PlanQueryError::UnsupportedOp(
                "WorstCaseOptimalJoin.var_len",
            ));
        }
    }
    Ok(())
}

fn resolve_edge_label_id(
    execution: &GqlExecutionContext,
    edge: &WcojEdge,
) -> Result<Option<EdgeLabelId>, PlanQueryError> {
    if edge.label_expr.is_some() {
        return Ok(None);
    }
    match edge.label.as_deref() {
        Some(label) => execution
            .resolved_edge_label_id(label)
            .map(Some)
            .ok_or_else(|| PlanQueryError::MissingResolvedLabel {
                namespace: "edge",
                name: label.to_owned(),
            }),
        None => Ok(None),
    }
}

fn single_hop_candidates(
    ctx: &ExecuteCtx<'_>,
    row: &PlanRow,
    edge: &WcojEdge,
    src_id: VertexId,
    required_dst: Option<VertexId>,
) -> Result<Vec<(VertexId, EdgeBinding)>, PlanQueryError> {
    let label_id = resolve_edge_label_id(&ctx.execution, edge)?;
    let mut candidates = Vec::new();
    expand_candidates_for_expand_op_into(
        ctx.store,
        &ctx.execution,
        src_id,
        edge.direction,
        label_id,
        edge.label_expr.as_ref(),
        EdgeSequenceOrder::Descending,
        edge.indexed_edge_equality.as_ref(),
        None,
        None,
        ctx.parameters,
        &mut candidates,
    )?;
    let mut out = Vec::new();
    for (edge_dst, edge_binding) in candidates {
        if let Some(expr) = edge.label_expr.as_ref()
            && !edge_binding_matches_label_expr(&ctx.execution, expr, &edge_binding)
        {
            continue;
        }
        let ExpandDst::Local(dst_id) = edge_dst else {
            continue;
        };
        if required_dst.is_some_and(|required| required != dst_id) {
            continue;
        }
        if !expand_dst_matches_prebound_vertex(row, &edge.dst, edge_dst) {
            continue;
        }
        out.push((dst_id, edge_binding));
    }
    Ok(out)
}

fn bind_wcoj_hop(
    store: &crate::facade::GraphStore,
    execution: &crate::gql_execution_context::GqlExecutionContext,
    row: &PlanRow,
    edge: &WcojEdge,
    dst_id: VertexId,
    edge_binding: EdgeBinding,
) -> Result<PlanRow, PlanQueryError> {
    let dst_binding = expand_dst_binding(store, execution, ExpandDst::Local(dst_id), None)?;
    let mut updates = vec![
        (edge.dst.as_ref(), dst_binding),
        (
            edge.variable.as_ref(),
            PlanBinding::Edge(edge_binding.clone()),
        ),
    ];
    if let Some(hop_key) = edge.hop_aux_binding.as_deref() {
        updates.push((hop_key, PlanBinding::Value(hop_aux_scalar(&edge_binding))));
    }
    Ok(row.fork(updates))
}

fn wcoj_search(
    ctx: &ExecuteCtx<'_>,
    partial_row: &PlanRow,
    edges: &[WcojEdge],
    hop: usize,
    anchor_vertex: VertexId,
    out: &mut Vec<PlanRow>,
) -> Result<(), PlanQueryError> {
    let edge = &edges[hop];
    let is_closing = hop + 1 == edges.len();
    let Some(src_id) = vertex_id_from_row(partial_row, edge.src.as_ref())? else {
        return Ok(());
    };
    let required_dst = is_closing.then_some(anchor_vertex);
    let candidates = single_hop_candidates(ctx, partial_row, edge, src_id, required_dst)?;
    let evaluator = ctx.expr_evaluator(None);
    for (dst_id, edge_binding) in candidates {
        let expanded = bind_wcoj_hop(
            ctx.store,
            &ctx.execution,
            partial_row,
            edge,
            dst_id,
            edge_binding,
        )?;
        if !row_matches_all(&evaluator, &expanded, &edge.dst_filter)? {
            continue;
        }
        if is_closing {
            out.push(expanded);
        } else {
            wcoj_search(ctx, &expanded, edges, hop + 1, anchor_vertex, out)?;
        }
    }
    Ok(())
}

fn vertex_id_from_row(row: &PlanRow, key: &str) -> Result<Option<VertexId>, PlanQueryError> {
    match row.get(key) {
        Some(PlanBinding::Vertex(id)) => Ok(Some(*id)),
        Some(PlanBinding::RemoteVertex(_)) => Err(PlanQueryError::UnsupportedOp(
            "WorstCaseOptimalJoin.remote_vertex",
        )),
        None => Ok(None),
        Some(binding) => Err(PlanQueryError::InvalidExpressionValue {
            expression: format!("WCOJ expected vertex binding for {key}, got {binding:?}"),
        }),
    }
}

pub(crate) async fn execute_wcoj(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    _variables: &[Str],
    edges: &[WcojEdge],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    ensure_wcoj_supported(edges)?;
    if edges.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for row in rows {
        let anchor_vertex = match vertex_id_from_row(&row, edges[0].src.as_ref())? {
            Some(id) => id,
            None => continue,
        };
        wcoj_search(ctx, &row, edges, 0, anchor_vertex, &mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use gleaph_gql_planner::plan::PlanOp;

    fn install_wcoj_triangle(store: &GraphStore, payload: &[u8]) -> (VertexId, VertexId, VertexId) {
        use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
        let label_id = crate::test_labels::edge_label_id_for_name("WcojTriRel");
        store
            .install_edge_label_payload_profile_at_init(
                label_id,
                EdgePayloadProfile {
                    byte_width: 2,
                    encoding: EdgePayloadEncoding::WeightRawU16,
                },
            )
            .unwrap();
        let a = store
            .insert_vertex_named(["WcojTriNode"], Vec::<(&str, Value)>::new())
            .expect("a");
        let b = store
            .insert_vertex_named(["WcojTriNode"], Vec::<(&str, Value)>::new())
            .expect("b");
        let c = store
            .insert_vertex_named(["WcojTriNode"], Vec::<(&str, Value)>::new())
            .expect("c");
        store
            .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), payload)
            .expect("a-b");
        store
            .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), payload)
            .expect("b-c");
        store
            .insert_directed_edge_with_payload_bytes(c, a, Some(label_id), payload)
            .expect("c-a");
        (a, b, c)
    }

    #[test]
    fn wcoj_triangle_finds_directed_cycle() {
        let store = GraphStore::new();
        let payload = 3u16.to_le_bytes();
        install_wcoj_triangle(&store, &payload);
        let plan = plan_gql(
            "MATCH (a:WcojTriNode)-[:WcojTriRel]->(b:WcojTriNode)-[:WcojTriRel]->(c:WcojTriNode)-[:WcojTriRel]->(a) \
             RETURN a",
        );
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::WorstCaseOptimalJoin { .. })),
            "expected WCOJ plan, ops={:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("wcoj triangle");
        assert!(
            !result.rows.is_empty(),
            "expected at least one triangle row"
        );
    }

    #[test]
    fn wcoj_triangle_hop_aux_returns_referenced_edge_payload() {
        let store = GraphStore::new();
        let payload = 9u16.to_le_bytes();
        install_wcoj_triangle(&store, &payload);
        let plan = plan_gql(
            "MATCH (a:WcojTriNode)-[e1:WcojTriRel]->(b:WcojTriNode)-[e2:WcojTriRel]->(c:WcojTriNode)-[e3:WcojTriRel]->(a) \
             RETURN e1__hop_aux AS aux",
        );
        let hop_aux = plan.ops.iter().find_map(|op| match op {
            PlanOp::WorstCaseOptimalJoin { edges, .. } => edges
                .iter()
                .find(|e| &*e.variable == "e1")
                .and_then(|e| e.hop_aux_binding.clone()),
            _ => None,
        });
        assert_eq!(hop_aux.as_deref().map(|s| s.as_ref()), Some("e1__hop_aux"));

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("wcoj hop_aux");
        assert!(!result.rows.is_empty());
        assert!(
            result
                .rows
                .iter()
                .all(|row| row.get("aux") == Some(&Value::Bytes(payload.to_vec()))),
            "rows={:?}",
            result.rows
        );
    }
}
