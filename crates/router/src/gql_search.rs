//! Router-side lowering of GQL `SEARCH ... IN (VECTOR INDEX ... FOR ... LIMIT ...)`
//! (ADR 0034 Slice 3).
//!
//! This module is the execution boundary between the provider-neutral GQL planner
//! (`PlanOp::Search`) and the Router-owned vector-index catalog / canister dispatch. It accepts
//! only a narrow leading shape, rejects everything else with explicit `InvalidArgument` errors, and
//! dispatches the remaining graph-tail plan from row-shaped vector-search seeds.

use candid::Encode;
use std::collections::BTreeMap;

use crate::planner_stats::RouterGraphStats;
use gleaph_gql::Value;
use gleaph_gql::ast::ExprKind;
use gleaph_gql_planner::plan::{
    NodeLabelRef, PhysicalPlan, PlanOp, SearchOutputKind, SearchProviderPlan, Str,
};
use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, GqlExecutionMode, GqlQueryResult, SeedBindingsWire, SeedFloat64Binding,
    SeedRowWire, SeedVertexBinding,
};
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_SEARCH_TOP_K, VectorMetric, VectorSearchHit, VectorSubject,
};

use crate::RouterStore;
use crate::canister::vector_search;
use crate::facade::stable::{embedding_name_catalog, graph_catalog, vector_index_catalog};
use crate::federation::{
    empty_execute_plan_result, federated_merge_mode_from_plans, merge_execute_plan_result,
};
use crate::graph_client::execute_plan_on_graph;
use crate::state::RouterError;
use crate::types::RouterVectorSearchRequest;

/// Execute a GQL plan whose leading ops are a supported `SEARCH` vector-search prefix.
///
/// Returns `Ok(None)` if the plan does not contain `PlanOp::Search` at all, allowing the caller
/// to fall through to the normal dispatch path. Returns `Err` for any `SEARCH` shape that is not
/// accepted by Slice 3.
pub(crate) async fn try_execute_gql_search(
    plan: &PhysicalPlan,
    graph_id: GraphId,
    params_blob: &[u8],
    mode: GqlExecutionMode,
    _stats: &RouterGraphStats,
    store: &RouterStore,
) -> Result<Option<GqlQueryResult>, RouterError> {
    if !gleaph_gql_planner::plan_contains_search(plan) {
        return Ok(None);
    }

    if mode != GqlExecutionMode::Query {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH lowering only supports query mode in this slice".into(),
        ));
    }

    let shape = analyze_search_shape(plan, graph_id, store)?;
    let params = gleaph_gql_ic::wire::decode_gql_params_blob(params_blob).map_err(|e| {
        RouterError::InvalidArgument(format!("failed to decode GQL parameters: {e}"))
    })?;

    let stripped_plan = strip_search_prefix(plan, &shape)?;
    if stripped_plan.has_dml() {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH cannot be followed by mutation operators in this slice".into(),
        ));
    }

    let (index_id, def) = resolve_vector_index(graph_id, &shape.index_name)?;
    let query = resolve_query_bytes(&shape.query_expr, &params)?;
    let top_k = resolve_limit_u32(&shape.limit_expr, &params)?;

    if top_k == 0 || top_k > MAX_VECTOR_SEARCH_TOP_K {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH LIMIT must be in 1..={MAX_VECTOR_SEARCH_TOP_K}"
        )));
    }
    let expected_bytes = def.encoding.stride_bytes(def.dims) as usize;
    if query.len() != expected_bytes {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH query byte length {} does not match dims*stride {}",
            query.len(),
            expected_bytes
        )));
    }

    if shape.output_kind == SearchOutputKind::Score && def.metric == VectorMetric::L2Squared {
        return Err(RouterError::InvalidArgument(
            "SEARCH SCORE AS is not supported for L2Squared in this slice".into(),
        ));
    }

    let logical_graph_name = graph_catalog::graph_name(graph_id)
        .ok_or_else(|| RouterError::NotFound(format!("graph id {graph_id}")))?;
    let req = RouterVectorSearchRequest {
        logical_graph_name,
        index_id,
        query,
        dims: def.dims,
        top_k,
    };
    let result = vector_search(req).await?;

    if result.hits.is_empty() {
        return Ok(Some(GqlQueryResult::row_count_only(0)));
    }

    let seeds_by_shard = build_search_seeds(
        &shape.binding,
        &shape.output_alias,
        &shape.required_label_ids,
        &result.hits,
    );

    let stripped_plan_blob =
        gleaph_gql_planner::wire::encode_block_plans(std::slice::from_ref(&stripped_plan), false)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

    dispatch_search_read_plan(
        graph_id,
        plan,
        &stripped_plan_blob,
        &stripped_plan,
        seeds_by_shard,
        params_blob,
        mode,
        _stats,
        store,
    )
    .await
    .map(Some)
}

struct SearchShape {
    binding: String,
    index_name: Vec<Str>,
    query_expr: gleaph_gql::ast::Expr,
    limit_expr: gleaph_gql::ast::Expr,
    output_alias: String,
    output_kind: SearchOutputKind,
    required_label_ids: Vec<VertexLabelId>,
}

fn analyze_search_shape(
    plan: &PhysicalPlan,
    graph_id: GraphId,
    store: &RouterStore,
) -> Result<SearchShape, RouterError> {
    let (node_scan, search, tail) = match plan.ops.as_slice() {
        [first, second, rest @ ..] => (first, second, rest),
        _ => {
            return Err(RouterError::InvalidArgument(
                "GQL SEARCH must be a leading NodeScan followed by Search".into(),
            ));
        }
    };

    let PlanOp::NodeScan {
        variable, label, ..
    } = node_scan
    else {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH requires a leading NodeScan".into(),
        ));
    };

    let PlanOp::Search {
        binding,
        provider,
        output,
    } = search
    else {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH second operator must be Search".into(),
        ));
    };

    if variable != binding {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH binding must match the leading NodeScan variable".into(),
        ));
    }

    if tail.iter().any(op_contains_search) {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH must appear exactly once as the leading prefix in this slice".into(),
        ));
    }

    let SearchProviderPlan::VectorIndex {
        index_name,
        query,
        limit,
        filter,
    } = provider;

    if filter.is_some() {
        return Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE is not supported yet".into(),
        ));
    }

    let required_label_ids = if let Some(label) = label {
        vec![resolve_vertex_label_id(graph_id, store, label)?]
    } else {
        Vec::new()
    };

    Ok(SearchShape {
        binding: binding.to_string(),
        index_name: index_name.clone(),
        query_expr: query.clone(),
        limit_expr: limit.clone(),
        output_alias: output.alias.to_string(),
        output_kind: output.kind,
        required_label_ids,
    })
}

fn op_contains_search(op: &PlanOp) -> bool {
    match op {
        PlanOp::Search { .. } => true,
        PlanOp::OptionalMatch { sub_plan }
        | PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => sub_plan.iter().any(op_contains_search),
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            left.iter().any(op_contains_search) || right.iter().any(op_contains_search)
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => sub_plan.ops.iter().any(op_contains_search),
        PlanOp::SetOperation { right, .. } => right.ops.iter().any(op_contains_search),
        _ => false,
    }
}

fn resolve_vertex_label_id(
    graph_id: GraphId,
    store: &RouterStore,
    label: &NodeLabelRef,
) -> Result<VertexLabelId, RouterError> {
    store
        .lookup_vertex_label_id(graph_id, label.as_ref())
        .map_err(|e| RouterError::InvalidArgument(format!("SEARCH label {}: {e}", label.as_ref())))
}

fn resolve_vector_index(
    graph_id: GraphId,
    index_name_parts: &[Str],
) -> Result<(u32, vector_index_catalog::VectorIndexDefRecord), RouterError> {
    let name = index_name_parts.join(".");
    let embedding_name_id = embedding_name_catalog::lookup_embedding_name_id(graph_id, &name)
        .ok_or_else(|| RouterError::NotFound(format!("vector index/embedding name {name}")))?;
    let def = vector_index_catalog::list_vector_indexes(graph_id)
        .into_iter()
        .find(|d| d.embedding_name_id == embedding_name_id)
        .ok_or_else(|| RouterError::NotFound(format!("vector index for embedding name {name}")))?;
    Ok((def.index_id, def))
}

fn resolve_query_bytes(
    expr: &gleaph_gql::ast::Expr,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<u8>, RouterError> {
    let value = match &expr.kind {
        ExprKind::Literal(v) => v.clone(),
        ExprKind::Parameter(name) => {
            let key = name.strip_prefix('$').unwrap_or(name.as_str());
            params
                .get(key)
                .ok_or_else(|| RouterError::InvalidArgument(format!("missing parameter ${name}")))?
                .clone()
        }
        _ => {
            return Err(RouterError::InvalidArgument(
                "SEARCH FOR must be a bytes literal or parameter".into(),
            ));
        }
    };
    match value {
        Value::Bytes(b) => Ok(b),
        _ => Err(RouterError::InvalidArgument(
            "SEARCH FOR must evaluate to bytes".into(),
        )),
    }
}

fn resolve_limit_u32(
    expr: &gleaph_gql::ast::Expr,
    params: &BTreeMap<String, Value>,
) -> Result<u32, RouterError> {
    let value = match &expr.kind {
        ExprKind::Literal(v) => v.clone(),
        ExprKind::Parameter(name) => {
            let key = name.strip_prefix('$').unwrap_or(name.as_str());
            params
                .get(key)
                .ok_or_else(|| RouterError::InvalidArgument(format!("missing parameter ${name}")))?
                .clone()
        }
        _ => {
            return Err(RouterError::InvalidArgument(
                "SEARCH LIMIT must be an integer literal or parameter".into(),
            ));
        }
    };
    let n: u64 = match value {
        Value::Int8(v) if v > 0 => v as u64,
        Value::Int16(v) if v > 0 => v as u64,
        Value::Int32(v) if v > 0 => v as u64,
        Value::Int64(v) if v > 0 => v as u64,
        Value::Uint8(v) if v > 0 => v as u64,
        Value::Uint16(v) if v > 0 => v as u64,
        Value::Uint32(v) if v > 0 => v as u64,
        Value::Uint64(v) if v > 0 => v,
        _ => {
            return Err(RouterError::InvalidArgument(
                "SEARCH LIMIT must be a positive integer".into(),
            ));
        }
    };
    if n > u32::MAX as u64 {
        return Err(RouterError::InvalidArgument(
            "SEARCH LIMIT exceeds u32::MAX".into(),
        ));
    }
    Ok(n as u32)
}

fn build_search_seeds(
    binding: &str,
    alias: &str,
    required_label_ids: &[VertexLabelId],
    hits: &[VectorSearchHit],
) -> BTreeMap<ShardId, SeedBindingsWire> {
    let mut by_shard: BTreeMap<ShardId, Vec<SeedRowWire>> = BTreeMap::new();
    for hit in hits {
        let VectorSubject::Vertex {
            shard_id,
            vertex_id,
        } = hit.subject;
        let row = SeedRowWire {
            vertex_bindings: vec![SeedVertexBinding {
                variable: binding.to_string(),
                local_vertex_id: vertex_id,
                required_vertex_label_ids: required_label_ids.iter().map(|l| l.raw()).collect(),
            }],
            float64_bindings: vec![SeedFloat64Binding {
                variable: alias.to_string(),
                value: f64::from(hit.distance),
            }],
        };
        by_shard.entry(shard_id).or_default().push(row);
    }
    by_shard
        .into_iter()
        .map(|(shard_id, rows)| {
            let wire = SeedBindingsWire {
                entries: Vec::new(),
                rows,
            };
            (shard_id, wire)
        })
        .collect()
}

fn strip_search_prefix(
    plan: &PhysicalPlan,
    _shape: &SearchShape,
) -> Result<PhysicalPlan, RouterError> {
    let tail = plan.ops[2..].to_vec();
    if tail.is_empty() {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH plan has no tail after the vector-search prefix".into(),
        ));
    }
    Ok(PhysicalPlan {
        ops: tail,
        diagnostics: plan.diagnostics.clone(),
        annotations: plan.annotations.clone(),
        output: plan.output.clone(),
        binding_layout: plan.binding_layout.clone(),
    })
}

async fn dispatch_search_read_plan(
    graph_id: GraphId,
    _original_plan: &PhysicalPlan,
    stripped_plan_blob: &[u8],
    stripped_plan: &PhysicalPlan,
    seeds_by_shard: BTreeMap<ShardId, SeedBindingsWire>,
    params_blob: &[u8],
    mode: GqlExecutionMode,
    _stats: &RouterGraphStats,
    store: &RouterStore,
) -> Result<GqlQueryResult, RouterError> {
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    if shards.is_empty() {
        return Err(RouterError::ShardNotRegistered);
    }

    let element_id_encoding_key = store.graph_element_id_encoding_key(graph_id)?.0;
    let resolved_labels =
        store.resolve_plan_labels(graph_id, std::slice::from_ref(stripped_plan))?;
    let resolved_properties =
        store.resolve_plan_properties(graph_id, std::slice::from_ref(stripped_plan))?;
    let indexed_properties =
        crate::index_catalog::graph_stats_for(graph_id).to_indexed_property_catalog();
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    let indexed_embeddings =
        vector_index_catalog::to_indexed_embedding_catalog(graph_id, dispatch_ready);

    let merge_mode = federated_merge_mode_from_plans(std::slice::from_ref(stripped_plan));

    let mut merged = empty_execute_plan_result();
    let mut dispatched_any = false;
    for shard in shards {
        let Some(seed_wire) = seeds_by_shard.get(&shard.shard_id) else {
            continue;
        };
        dispatched_any = true;
        let seed_blob = Encode!(seed_wire).expect("encode search seed bindings");
        let result = execute_plan_on_graph(
            shard.graph_canister,
            ExecutePlanArgs {
                target_shard_id: shard.shard_id,
                element_id_encoding_key,
                mutation_id: None,
                plan_blob: stripped_plan_blob.to_vec(),
                params_blob: params_blob.to_vec(),
                mode,
                seed_bindings_blob: Some(seed_blob),
                resolved_labels: Some(resolved_labels.clone()),
                resolved_properties: Some(resolved_properties.clone()),
                indexed_properties: Some(indexed_properties.clone()),
                unique_claims: None,
                constrained_properties: None,
                local_unique_claims: None,
                local_constrained_properties: None,
                indexed_embeddings: Some(indexed_embeddings.clone()),
            },
        )
        .await
        .map_err(RouterError::InvalidArgument)?;
        merge_execute_plan_result(&mut merged, result, merge_mode.clone())
            .map_err(RouterError::InvalidArgument)?;
    }

    if !dispatched_any {
        // All vector hits landed on shards that are no longer live. Return empty rather than
        // fail-closed; the hits are not graph-visible from the current shard topology.
        return Ok(GqlQueryResult::row_count_only(0));
    }

    Ok(GqlQueryResult::from_merged(&merged))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;
    use gleaph_gql::ast::{Expr, ExprKind};
    use gleaph_gql_planner::plan::{SearchOutputKind, SearchOutputPlan, SearchProviderPlan};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{VectorSearchHit, VectorSubject};
    use std::collections::BTreeMap;

    fn bytes_expr(b: Vec<u8>) -> Expr {
        Expr::new(ExprKind::Literal(Value::Bytes(b)))
    }

    fn param_expr(name: &str) -> Expr {
        Expr::new(ExprKind::Parameter(name.to_owned()))
    }

    fn search_plan_with_distance() -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3]),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ])
    }

    #[test]
    fn resolve_query_bytes_strips_dollar_prefix_for_router_params() {
        let params = BTreeMap::from([("query".to_string(), Value::Bytes(vec![4, 5, 6]))]);
        assert_eq!(
            resolve_query_bytes(&param_expr("$query"), &params).unwrap(),
            vec![4, 5, 6]
        );
    }

    #[test]
    fn resolve_limit_u32_strips_dollar_prefix_for_router_params() {
        let params = BTreeMap::from([("k".to_string(), Value::Int64(42))]);
        assert_eq!(resolve_limit_u32(&param_expr("$k"), &params).unwrap(), 42);
    }

    #[test]
    fn try_execute_gql_search_rejects_non_query_mode() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3]),
                    limit: Expr::int(1),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            GraphId::from_raw(0),
            &[],
            GqlExecutionMode::Update,
            &RouterGraphStats::from_catalog(
                GraphId::from_raw(0),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
        ));
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("only supports query mode"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_rejects_dml_tail() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3]),
                    limit: Expr::int(1),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec!["Doc".into()],
                properties: vec![],
            },
        ]);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            GraphId::from_raw(0),
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                GraphId::from_raw(0),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
        ));
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot be followed by mutation operators"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_query_bytes_accepts_literal_and_parameter() {
        let params = BTreeMap::from([("q".to_string(), Value::Bytes(vec![4, 5, 6]))]);
        assert_eq!(
            resolve_query_bytes(&bytes_expr(vec![1, 2, 3]), &params).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            resolve_query_bytes(&param_expr("q"), &params).unwrap(),
            vec![4, 5, 6]
        );
    }

    #[test]
    fn resolve_query_bytes_rejects_non_bytes() {
        let params = BTreeMap::from([("q".to_string(), Value::Int64(42))]);
        let err = resolve_query_bytes(&param_expr("q"), &params).unwrap_err();
        assert!(err.to_string().contains("must evaluate to bytes"));
    }

    #[test]
    fn resolve_limit_u32_accepts_literal_and_parameter() {
        let params = BTreeMap::from([("k".to_string(), Value::Int64(25))]);
        assert_eq!(resolve_limit_u32(&Expr::int(10), &params).unwrap(), 10);
        assert_eq!(resolve_limit_u32(&param_expr("k"), &params).unwrap(), 25);
    }

    #[test]
    fn resolve_limit_u32_rejects_non_positive_and_overflow() {
        let params = BTreeMap::new();
        assert!(resolve_limit_u32(&Expr::int(0), &params).is_err());
        assert!(resolve_limit_u32(&Expr::int(-1), &params).is_err());
        assert!(resolve_limit_u32(&bytes_expr(vec![]), &params).is_err());
    }

    #[test]
    fn build_search_seeds_groups_hits_by_shard_and_carries_distance_alias() {
        let hits = vec![
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                distance: 1.25f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(1),
                    vertex_id: 9,
                },
                distance: 2.5f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
        ];
        let by_shard = build_search_seeds("d", "distance", &[], &hits);
        assert_eq!(by_shard.len(), 2);

        let shard0 = by_shard.get(&ShardId::new(0)).unwrap();
        assert_eq!(shard0.rows.len(), 1);
        assert_eq!(shard0.rows[0].vertex_bindings[0].local_vertex_id, 7);
        assert_eq!(shard0.rows[0].float64_bindings[0].value, 1.25f64);

        let shard1 = by_shard.get(&ShardId::new(1)).unwrap();
        assert_eq!(shard1.rows[0].vertex_bindings[0].local_vertex_id, 9);
        assert_eq!(shard1.rows[0].float64_bindings[0].value, 2.5f64);
    }

    #[test]
    fn build_search_seeds_includes_required_label_ids() {
        let hits = vec![VectorSearchHit {
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 5,
            },
            distance: 0.0f32,
            embedding_incarnation: 0,
            embedding_version: 0,
        }];
        let by_shard = build_search_seeds("d", "distance", &[VertexLabelId::from_raw(3)], &hits);
        assert_eq!(
            by_shard[&ShardId::new(0)].rows[0].vertex_bindings[0].required_vertex_label_ids,
            vec![3]
        );
    }

    #[test]
    fn strip_search_prefix_removes_node_scan_and_search() {
        let plan = search_plan_with_distance();
        let shape = SearchShape {
            binding: "d".into(),
            index_name: vec!["doc_vec".into()],
            query_expr: bytes_expr(vec![]),
            limit_expr: Expr::int(1),
            output_alias: "distance".into(),
            output_kind: SearchOutputKind::Distance,
            required_label_ids: vec![],
        };
        let stripped = strip_search_prefix(&plan, &shape).unwrap();
        assert_eq!(stripped.ops.len(), 1);
        assert!(matches!(stripped.ops[0], PlanOp::Project { .. }));
    }

    #[test]
    fn strip_search_prefix_rejects_empty_tail() {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![]),
                    limit: Expr::int(1),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
        ]);
        let shape = SearchShape {
            binding: "d".into(),
            index_name: vec!["doc_vec".into()],
            query_expr: bytes_expr(vec![]),
            limit_expr: Expr::int(1),
            output_alias: "distance".into(),
            output_kind: SearchOutputKind::Distance,
            required_label_ids: vec![],
        };
        let err = strip_search_prefix(&plan, &shape).unwrap_err();
        assert!(err.to_string().contains("no tail"));
    }

    #[test]
    fn op_contains_search_detects_nested_search() {
        let inner_search = PhysicalPlan::from_ops(vec![PlanOp::Search {
            binding: "d".into(),
            provider: SearchProviderPlan::VectorIndex {
                index_name: vec!["doc_vec".into()],
                query: bytes_expr(vec![]),
                limit: Expr::int(1),
                filter: None,
            },
            output: SearchOutputPlan {
                kind: SearchOutputKind::Distance,
                alias: "distance".into(),
            },
        }]);
        assert!(op_contains_search(&PlanOp::OptionalMatch {
            sub_plan: inner_search.ops.clone(),
        }));
        assert!(op_contains_search(&PlanOp::UseGraph {
            graph_name: vec!["g".into()],
            sub_plan: Some(inner_search.ops),
        }));
        assert!(!op_contains_search(&PlanOp::NodeScan {
            variable: "n".into(),
            label: None,
            property_projection: None,
        }));
    }
}
