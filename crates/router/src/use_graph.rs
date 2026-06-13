//! UseGraph ingress routing (ADR 0011 U1a/U1b/U2).

use candid::Principal;
use gleaph_gql::ast::{
    CompositeQueryExpr, GqlProgram, ObjectName, SimpleQueryStatement, Statement, StatementBlock,
};
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_planner::plan::Str;
use gleaph_gql_planner::{
    PhysicalPlan, PlanOp, analyze_remote_use_graph_pushdown, build_block_plan_with_schema,
};
use gleaph_graph_kernel::entry::GraphId;

use crate::facade::store::RouterStore;
use crate::graph_context;
use crate::index_catalog::graph_stats_for;
use crate::state::RouterError;

/// Resolved planning/dispatch target for GQL ingress.
#[derive(Clone, Debug, PartialEq)]
pub struct IngressDispatch {
    pub dispatch_graph_id: GraphId,
    pub plan_block: StatementBlock,
}

/// One graph-local fragment after v2 `USE GRAPH` analysis.
#[derive(Clone, Debug)]
pub struct UseGraphSegment {
    pub graph_id: GraphId,
    pub ops: Vec<PlanOp>,
}

/// Router-side routing decision for multi-graph `USE GRAPH` (ADR 0011 U2).
#[derive(Clone, Debug)]
pub enum UseGraphV2Dispatch {
    /// Plan executes on the ingress dispatch graph (no remote focused graphs).
    EffectiveGraph { plan: PhysicalPlan },
    /// Nested or single remote `USE GRAPH` peeled to one target graph.
    Single {
        graph_id: GraphId,
        plan: PhysicalPlan,
    },
    /// Sequential top-level `UseGraph` segments merged at router (union rows).
    Multi {
        segments: Vec<UseGraphSegment>,
        plan: PhysicalPlan,
    },
    /// Two focused graphs joined at router (cartesian product or hash join).
    Join {
        left: UseGraphSegment,
        right: UseGraphSegment,
        join: MultiGraphJoinKind,
        tail_ops: Vec<PlanOp>,
        plan: PhysicalPlan,
    },
}

/// How two graph-local fragments combine on the router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiGraphJoinKind {
    Cartesian,
    HashJoin { join_keys: Vec<Str> },
}

/// Resolve which graph and statement block to plan/dispatch.
pub fn resolve_ingress_dispatch(
    store: &RouterStore,
    program: &GqlProgram,
    block: &StatementBlock,
    caller: Principal,
    session_effective: GraphId,
) -> Result<IngressDispatch, RouterError> {
    let session_current = graph_context::session_current_after_activity(store, program, caller)?;

    if let Some((graph_name, defocused_block)) = try_defocus_top_level_use_graph(block) {
        let focused_id = graph_context::resolve_graph_reference_for_plan(
            store,
            &graph_name,
            caller,
            session_current,
            session_effective,
        )?;
        let stats = graph_stats_for(focused_id);
        let open = NoSchema;
        let mut typed = None;
        let schema = crate::facade::stable::graph_type_catalog::property_schema_for_planning(
            focused_id, &open, &mut typed,
        )?;
        let plan = build_block_plan_with_schema(&defocused_block, Some(&stats), schema)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        if plan.has_dml() {
            return Err(RouterError::InvalidArgument(
                "DML in remote USE GRAPH is not supported".into(),
            ));
        }
        if focused_id != session_effective {
            let pushdown =
                analyze_remote_use_graph_pushdown(&graph_name.parts.join("."), &plan.ops);
            if !pushdown.supported {
                return Err(RouterError::InvalidArgument(
                    pushdown
                        .reason
                        .unwrap_or_else(|| "remote USE GRAPH is not supported".into()),
                ));
            }
        }
        return Ok(IngressDispatch {
            dispatch_graph_id: focused_id,
            plan_block: defocused_block,
        });
    }

    Ok(IngressDispatch {
        dispatch_graph_id: session_effective,
        plan_block: block.clone(),
    })
}

/// Analyze a built physical plan for multi-graph `USE GRAPH` routing (U2).
pub fn analyze_use_graph_v2_dispatch(
    plan: PhysicalPlan,
    store: &RouterStore,
    caller: Principal,
    session_current: Option<GraphId>,
    session_effective: GraphId,
) -> Result<UseGraphV2Dispatch, RouterError> {
    if collect_use_graph_names(&plan.ops).is_empty() {
        return Ok(UseGraphV2Dispatch::EffectiveGraph { plan });
    }

    if let Some((chain, inner_ops)) = try_peel_use_graph_chain(&plan.ops)
        && collect_use_graph_names(&inner_ops).is_empty()
    {
        let graph_id = resolve_use_graph_chain_target(
            store,
            caller,
            session_current,
            session_effective,
            &chain,
            &inner_ops,
        )?;
        let defocused = defocused_plan_from_ops(plan, inner_ops);
        return Ok(UseGraphV2Dispatch::Single {
            graph_id,
            plan: defocused,
        });
    }

    if let Some((segments, tail_ops)) = try_split_use_graph_segments(&plan.ops) {
        let resolved = resolve_use_graph_segments(
            store,
            caller,
            session_current,
            session_effective,
            segments,
        )?;
        if tail_ops.is_empty() {
            return Ok(UseGraphV2Dispatch::Multi {
                segments: resolved,
                plan,
            });
        }
        if resolved.len() != 2 {
            return Err(RouterError::InvalidArgument(
                "multi-graph USE GRAPH tail ops require exactly two focused graphs".into(),
            ));
        }
        let [left, right] = [resolved[0].clone(), resolved[1].clone()];
        return Ok(UseGraphV2Dispatch::Join {
            left,
            right,
            join: MultiGraphJoinKind::Cartesian,
            tail_ops,
            plan,
        });
    }

    if let Some((left, right, join, tail_ops)) = try_extract_root_binary_join(&plan.ops) {
        let left_segment =
            resolve_use_graph_branch(store, caller, session_current, session_effective, left)?;
        let right_segment =
            resolve_use_graph_branch(store, caller, session_current, session_effective, right)?;
        if left_segment.graph_id == right_segment.graph_id {
            return Ok(UseGraphV2Dispatch::EffectiveGraph { plan });
        }
        return Ok(UseGraphV2Dispatch::Join {
            left: left_segment,
            right: right_segment,
            join,
            tail_ops,
            plan,
        });
    }

    if let Some(segments) = try_split_sequential_use_graph_segments(&plan.ops) {
        let resolved = resolve_use_graph_segments(
            store,
            caller,
            session_current,
            session_effective,
            segments,
        )?;
        return Ok(UseGraphV2Dispatch::Multi {
            segments: resolved,
            plan,
        });
    }

    if all_use_graphs_match_effective(store, caller, session_current, session_effective, &plan.ops)?
    {
        return Ok(UseGraphV2Dispatch::EffectiveGraph { plan });
    }

    Err(RouterError::InvalidArgument(
        "multi-graph USE GRAPH is not supported for this plan shape".into(),
    ))
}

fn try_defocus_top_level_use_graph(block: &StatementBlock) -> Option<(ObjectName, StatementBlock)> {
    if !block.next.is_empty() {
        return None;
    }
    let Statement::Query(cq) = &block.first else {
        return None;
    };
    if !cq.rest.is_empty() {
        return None;
    }
    let lq = &cq.left;
    let SimpleQueryStatement::Focused {
        graph,
        body: Some(body),
    } = lq.parts.first()?
    else {
        return None;
    };
    if lq.parts.len() != 1 {
        return None;
    }
    let inner_lq = gleaph_gql::ast::LinearQueryStatement {
        span: lq.span,
        at_schema: lq.at_schema.clone(),
        prefix_bindings: lq.prefix_bindings.clone(),
        parts: vec![(*body.clone())],
        result: lq.result.clone(),
    };
    Some((
        graph.clone(),
        StatementBlock {
            span: block.span,
            first: Statement::Query(Box::new(CompositeQueryExpr::single(inner_lq))),
            next: vec![],
        },
    ))
}

fn object_name_from_parts(parts: &[String]) -> ObjectName {
    ObjectName {
        parts: parts.to_vec(),
    }
}

pub(crate) fn defocused_plan_from_ops(plan: PhysicalPlan, ops: Vec<PlanOp>) -> PhysicalPlan {
    let mut defocused = PhysicalPlan::from_ops(ops);
    defocused.diagnostics = plan.diagnostics;
    defocused
}

fn resolve_use_graph_chain_target(
    store: &RouterStore,
    caller: Principal,
    session_current: Option<GraphId>,
    session_effective: GraphId,
    chain: &[Vec<String>],
    inner_ops: &[PlanOp],
) -> Result<GraphId, RouterError> {
    let inner_name = chain
        .last()
        .ok_or_else(|| RouterError::InvalidArgument("empty USE GRAPH chain".into()))?;
    let name = object_name_from_parts(inner_name);
    let graph_id = graph_context::resolve_graph_reference_for_plan(
        store,
        &name,
        caller,
        session_current,
        session_effective,
    )?;
    validate_remote_use_graph_segment(
        store,
        caller,
        session_current,
        session_effective,
        graph_id,
        inner_name,
        inner_ops,
    )?;
    Ok(graph_id)
}

fn validate_remote_use_graph_segment(
    store: &RouterStore,
    caller: Principal,
    session_current: Option<GraphId>,
    session_effective: GraphId,
    graph_id: GraphId,
    graph_name_parts: &[String],
    ops: &[PlanOp],
) -> Result<(), RouterError> {
    let _ = (store, caller, session_current);
    if ops.is_empty() {
        return Err(RouterError::InvalidArgument(
            "empty USE GRAPH sub-plan".into(),
        ));
    }
    if plan_has_dml_ops(ops) {
        return Err(RouterError::InvalidArgument(
            "DML in remote USE GRAPH is not supported".into(),
        ));
    }
    if graph_id != session_effective {
        let graph_name = graph_name_parts.join(".");
        let pushdown = analyze_remote_use_graph_pushdown(&graph_name, ops);
        if !pushdown.supported {
            return Err(RouterError::InvalidArgument(
                pushdown
                    .reason
                    .unwrap_or_else(|| "remote USE GRAPH is not supported".into()),
            ));
        }
    }
    Ok(())
}

fn plan_has_dml_ops(ops: &[PlanOp]) -> bool {
    PhysicalPlan::from_ops(ops.to_vec()).has_dml()
}

fn all_use_graphs_match_effective(
    store: &RouterStore,
    caller: Principal,
    session_current: Option<GraphId>,
    session_effective: GraphId,
    ops: &[PlanOp],
) -> Result<bool, RouterError> {
    for parts in collect_use_graph_names(ops) {
        let name = object_name_from_parts(&parts);
        let focused = graph_context::resolve_graph_reference_for_plan(
            store,
            &name,
            caller,
            session_current,
            session_effective,
        )?;
        if focused != session_effective {
            return Ok(false);
        }
    }
    Ok(true)
}

fn try_peel_use_graph_chain(ops: &[PlanOp]) -> Option<(Vec<Vec<String>>, Vec<PlanOp>)> {
    let mut chain = Vec::new();
    let mut current = ops;
    loop {
        if current.len() != 1 {
            break;
        }
        match &current[0] {
            PlanOp::InlineProcedureCall { sub_plan, .. } => current = &sub_plan.ops,
            PlanOp::UseGraph {
                graph_name,
                sub_plan: Some(sub),
            } => {
                chain.push(graph_name.iter().map(|s| s.to_string()).collect());
                current = sub;
            }
            _ => break,
        }
    }
    if chain.is_empty() {
        None
    } else {
        Some((chain, current.to_vec()))
    }
}

fn resolve_use_graph_segments(
    store: &RouterStore,
    caller: Principal,
    session_current: Option<GraphId>,
    session_effective: GraphId,
    segments: Vec<(Vec<String>, Vec<PlanOp>)>,
) -> Result<Vec<UseGraphSegment>, RouterError> {
    segments
        .into_iter()
        .map(|(graph_name, ops)| {
            resolve_use_graph_branch_with_name(
                store,
                caller,
                session_current,
                session_effective,
                graph_name,
                ops,
            )
        })
        .collect()
}

fn resolve_use_graph_branch(
    store: &RouterStore,
    caller: Principal,
    session_current: Option<GraphId>,
    session_effective: GraphId,
    branch_ops: Vec<PlanOp>,
) -> Result<UseGraphSegment, RouterError> {
    let (graph_name, ops) = peel_branch_use_graph(&branch_ops).ok_or_else(|| {
        RouterError::InvalidArgument("multi-graph branch missing focused USE GRAPH scope".into())
    })?;
    resolve_use_graph_branch_with_name(
        store,
        caller,
        session_current,
        session_effective,
        graph_name,
        ops,
    )
}

fn resolve_use_graph_branch_with_name(
    store: &RouterStore,
    caller: Principal,
    session_current: Option<GraphId>,
    session_effective: GraphId,
    graph_name: Vec<String>,
    ops: Vec<PlanOp>,
) -> Result<UseGraphSegment, RouterError> {
    let name = object_name_from_parts(&graph_name);
    let graph_id = graph_context::resolve_graph_reference_for_plan(
        store,
        &name,
        caller,
        session_current,
        session_effective,
    )?;
    validate_remote_use_graph_segment(
        store,
        caller,
        session_current,
        session_effective,
        graph_id,
        &graph_name,
        &ops,
    )?;
    Ok(UseGraphSegment { graph_id, ops })
}

fn peel_branch_use_graph(ops: &[PlanOp]) -> Option<(Vec<String>, Vec<PlanOp>)> {
    if let Some((chain, inner)) = try_peel_use_graph_chain(ops)
        && collect_use_graph_names(&inner).is_empty()
    {
        return Some((chain.last()?.clone(), inner));
    }
    let PlanOp::UseGraph {
        graph_name,
        sub_plan: Some(sub),
    } = ops.first()?
    else {
        return None;
    };
    if ops.len() != 1 || !collect_use_graph_names(sub).is_empty() {
        return None;
    }
    Some((
        graph_name.iter().map(|s| s.to_string()).collect(),
        sub.clone(),
    ))
}

fn extract_use_graph_segment_op(op: &PlanOp) -> Option<(Vec<String>, Vec<PlanOp>)> {
    match op {
        PlanOp::UseGraph {
            graph_name,
            sub_plan: Some(sub),
        } => Some((
            graph_name.iter().map(|s| s.to_string()).collect(),
            sub.clone(),
        )),
        PlanOp::InlineProcedureCall { sub_plan, .. } => peel_branch_use_graph(&sub_plan.ops),
        _ => None,
    }
}

fn try_split_use_graph_segments(
    ops: &[PlanOp],
) -> Option<(Vec<(Vec<String>, Vec<PlanOp>)>, Vec<PlanOp>)> {
    let mut segments = Vec::new();
    let mut idx = 0;
    while idx < ops.len() {
        let Some((graph_name, segment_ops)) = extract_use_graph_segment_op(&ops[idx]) else {
            break;
        };
        if collect_use_graph_names(&segment_ops).is_empty() {
            segments.push((graph_name, segment_ops));
            idx += 1;
        } else {
            return None;
        }
    }
    if segments.len() < 2 {
        return None;
    }
    Some((segments, ops[idx..].to_vec()))
}

fn try_extract_root_binary_join(
    ops: &[PlanOp],
) -> Option<(Vec<PlanOp>, Vec<PlanOp>, MultiGraphJoinKind, Vec<PlanOp>)> {
    let (left, right, join) = match ops.first()? {
        PlanOp::HashJoin {
            left,
            right,
            join_keys,
        } => (
            left.clone(),
            right.clone(),
            MultiGraphJoinKind::HashJoin {
                join_keys: join_keys.clone(),
            },
        ),
        PlanOp::CartesianProduct { left, right } => {
            (left.clone(), right.clone(), MultiGraphJoinKind::Cartesian)
        }
        _ => return None,
    };
    if collect_use_graph_names(&left).is_empty() && collect_use_graph_names(&right).is_empty() {
        return None;
    }
    Some((left, right, join, ops[1..].to_vec()))
}

fn try_split_sequential_use_graph_segments(
    ops: &[PlanOp],
) -> Option<Vec<(Vec<String>, Vec<PlanOp>)>> {
    if ops.len() < 2 {
        return None;
    }
    let mut segments = Vec::with_capacity(ops.len());
    for op in ops {
        let PlanOp::UseGraph {
            graph_name,
            sub_plan: Some(sub),
        } = op
        else {
            return None;
        };
        if !collect_use_graph_names(sub).is_empty() {
            return None;
        }
        segments.push((
            graph_name.iter().map(|s| s.to_string()).collect(),
            sub.clone(),
        ));
    }
    Some(segments)
}

fn collect_use_graph_names(ops: &[PlanOp]) -> Vec<Vec<String>> {
    let mut names = Vec::new();
    collect_use_graph_names_in_ops(ops, &mut names);
    names
}

fn collect_use_graph_names_in_ops(ops: &[PlanOp], out: &mut Vec<Vec<String>>) {
    for op in ops {
        match op {
            PlanOp::UseGraph {
                graph_name,
                sub_plan: Some(sub_plan),
            } => {
                out.push(graph_name.iter().map(|s| s.to_string()).collect());
                collect_use_graph_names_in_ops(sub_plan, out);
            }
            PlanOp::UseGraph { graph_name, .. } => {
                out.push(graph_name.iter().map(|s| s.to_string()).collect());
            }
            PlanOp::HashJoin { left, right, .. } => {
                collect_use_graph_names_in_ops(left, out);
                collect_use_graph_names_in_ops(right, out);
            }
            PlanOp::CartesianProduct { left, right } => {
                collect_use_graph_names_in_ops(left, out);
                collect_use_graph_names_in_ops(right, out);
            }
            PlanOp::SetOperation { right, .. } => {
                collect_use_graph_names_in_ops(&right.ops, out);
            }
            PlanOp::OptionalMatch { sub_plan } => collect_use_graph_names_in_ops(sub_plan, out),
            PlanOp::InlineProcedureCall { sub_plan, .. } => {
                collect_use_graph_names_in_ops(&sub_plan.ops, out);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::parser;
    use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};

    use crate::facade::stable::graph_catalog;

    fn register_graph(store: &RouterStore, name: &str, is_home: bool) {
        let owner = Principal::anonymous();
        store.bootstrap_controllers(&[owner]);
        store
            .admin_register_graph(
                owner,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    graph_name: name.to_owned(),
                    canister_id: Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home,
                },
            )
            .expect("register");
    }

    fn block_for_query(query: &str) -> (GqlProgram, StatementBlock) {
        let program = parser::parse(query).expect("parse");
        let block = program
            .transaction_activity
            .as_ref()
            .and_then(|tx| tx.body.clone())
            .expect("block");
        (program, block)
    }

    #[test]
    fn remote_use_graph_defocuses_to_focused_graph() {
        let store = RouterStore::new();
        register_graph(&store, "tenant_a", true);
        register_graph(&store, "tenant_b", false);
        let caller = Principal::anonymous();
        let query = "SESSION SET GRAPH tenant_a USE tenant_b MATCH (n) RETURN n";
        let (program, block) = block_for_query(query);
        let effective = graph_context::resolve_graph_context(&store, &program, caller)
            .expect("resolve")
            .graph_id;
        let dispatch = resolve_ingress_dispatch(&store, &program, &block, caller, effective)
            .expect("dispatch");
        assert_eq!(
            graph_catalog::lookup_graph_id("tenant_b"),
            Some(dispatch.dispatch_graph_id)
        );
        let stats = graph_stats_for(dispatch.dispatch_graph_id);
        let plan = build_block_plan_with_schema(&dispatch.plan_block, Some(&stats), &NoSchema)
            .expect("plan");
        assert!(
            !plan
                .ops
                .iter()
                .any(|op| matches!(op, PlanOp::UseGraph { .. })),
            "defocused plan should not contain UseGraph: {:?}",
            plan.ops
        );
    }

    #[test]
    fn nested_use_graph_in_inline_call_defocuses_to_innermost() {
        let store = RouterStore::new();
        register_graph(&store, "tenant_a", true);
        register_graph(&store, "tenant_b", false);
        let caller = Principal::anonymous();
        let query = "USE tenant_a { USE tenant_b { MATCH (n) RETURN n } }";
        let (program, block) = block_for_query(query);
        let effective = graph_context::resolve_graph_context(&store, &program, caller)
            .expect("resolve")
            .graph_id;
        let dispatch = resolve_ingress_dispatch(&store, &program, &block, caller, effective)
            .expect("dispatch");
        assert_eq!(dispatch.dispatch_graph_id, effective);
        let stats = graph_stats_for(dispatch.dispatch_graph_id);
        let plan = build_block_plan_with_schema(&dispatch.plan_block, Some(&stats), &NoSchema)
            .expect("plan");
        let session_current =
            graph_context::session_current_after_activity(&store, &program, caller).expect("sess");
        let v2 = analyze_use_graph_v2_dispatch(plan, &store, caller, session_current, effective)
            .expect("v2");
        let UseGraphV2Dispatch::Single { graph_id, plan } = v2 else {
            panic!("expected single-graph v2 dispatch, got {v2:?}");
        };
        assert_eq!(graph_catalog::lookup_graph_id("tenant_b"), Some(graph_id));
        assert!(
            !plan
                .ops
                .iter()
                .any(|op| matches!(op, PlanOp::UseGraph { .. })),
            "defocused plan should not contain UseGraph: {:?}",
            plan.ops
        );
    }

    #[test]
    fn same_graph_use_defocuses_without_mismatch_error() {
        let store = RouterStore::new();
        register_graph(&store, "tenant_a", false);
        let caller = Principal::anonymous();
        let query = "SESSION SET GRAPH tenant_a USE tenant_a MATCH (n) RETURN n";
        let (program, block) = block_for_query(query);
        let effective = graph_context::resolve_graph_context(&store, &program, caller)
            .expect("resolve")
            .graph_id;
        resolve_ingress_dispatch(&store, &program, &block, caller, effective).expect("ok");
    }

    #[test]
    fn cross_graph_cartesian_product_dispatch() {
        let store = RouterStore::new();
        register_graph(&store, "tenant_a", true);
        register_graph(&store, "tenant_b", false);
        let query = "USE tenant_a MATCH (a) USE tenant_b MATCH (b) RETURN a, b";
        let (program, block) = block_for_query(query);
        let caller = Principal::anonymous();
        let effective = graph_context::resolve_graph_context(&store, &program, caller)
            .expect("resolve")
            .graph_id;
        let dispatch = resolve_ingress_dispatch(&store, &program, &block, caller, effective)
            .expect("dispatch");
        let stats = graph_stats_for(dispatch.dispatch_graph_id);
        let plan = build_block_plan_with_schema(&dispatch.plan_block, Some(&stats), &NoSchema)
            .expect("plan");
        let session_current =
            graph_context::session_current_after_activity(&store, &program, caller).expect("sess");
        let v2 = analyze_use_graph_v2_dispatch(plan, &store, caller, session_current, effective)
            .expect("v2");
        let UseGraphV2Dispatch::Join {
            left,
            right,
            join,
            tail_ops,
            ..
        } = v2
        else {
            panic!("expected join dispatch, got {v2:?}");
        };
        assert_eq!(
            graph_catalog::lookup_graph_id("tenant_a"),
            Some(left.graph_id)
        );
        assert_eq!(
            graph_catalog::lookup_graph_id("tenant_b"),
            Some(right.graph_id)
        );
        assert_eq!(join, MultiGraphJoinKind::Cartesian);
        assert!(matches!(tail_ops.as_slice(), [PlanOp::Project { .. }]));
    }

    #[test]
    fn next_remote_use_graph_segments_union() {
        let store = RouterStore::new();
        register_graph(&store, "tenant_a", true);
        register_graph(&store, "tenant_b", false);
        let query = "USE tenant_a { MATCH (n) RETURN n } NEXT USE tenant_b { MATCH (m) RETURN m }";
        let (program, block) = block_for_query(query);
        let caller = Principal::anonymous();
        let effective = graph_context::resolve_graph_context(&store, &program, caller)
            .expect("resolve")
            .graph_id;
        let dispatch = resolve_ingress_dispatch(&store, &program, &block, caller, effective)
            .expect("dispatch");
        let stats = graph_stats_for(dispatch.dispatch_graph_id);
        let plan = build_block_plan_with_schema(&dispatch.plan_block, Some(&stats), &NoSchema)
            .expect("plan");
        let session_current =
            graph_context::session_current_after_activity(&store, &program, caller).expect("sess");
        let v2 = analyze_use_graph_v2_dispatch(plan, &store, caller, session_current, effective)
            .expect("v2");
        let UseGraphV2Dispatch::Multi { segments, .. } = v2 else {
            panic!("expected multi-graph union, got {v2:?}");
        };
        assert_eq!(segments.len(), 2);
        assert_eq!(
            graph_catalog::lookup_graph_id("tenant_a"),
            Some(segments[0].graph_id)
        );
        assert_eq!(
            graph_catalog::lookup_graph_id("tenant_b"),
            Some(segments[1].graph_id)
        );
    }
}
