//! UseGraph ingress routing (ADR 0011 U1a/U1b).

use candid::Principal;
use gleaph_gql::ast::{
    CompositeQueryExpr, GqlProgram, ObjectName, SimpleQueryStatement, Statement, StatementBlock,
};
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_planner::{PlanOp, analyze_remote_use_graph_pushdown, build_block_plan_with_schema};
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
        let plan = build_block_plan_with_schema(&defocused_block, Some(&stats), &NoSchema)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        if plan.has_dml() {
            return Err(RouterError::InvalidArgument(
                "DML in remote USE GRAPH is not supported".into(),
            ));
        }
        if !collect_use_graph_names(&plan.ops).is_empty() {
            return Err(RouterError::InvalidArgument(
                "nested USE GRAPH is not supported".into(),
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

    ensure_no_mismatched_use_graph(store, program, block, caller, session_effective)?;
    Ok(IngressDispatch {
        dispatch_graph_id: session_effective,
        plan_block: block.clone(),
    })
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

fn ensure_no_mismatched_use_graph(
    store: &RouterStore,
    program: &GqlProgram,
    block: &StatementBlock,
    caller: Principal,
    session_effective: GraphId,
) -> Result<(), RouterError> {
    let session_current = graph_context::session_current_after_activity(store, program, caller)?;
    let stats = graph_stats_for(session_effective);
    let plan = build_block_plan_with_schema(block, Some(&stats), &NoSchema)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    for parts in collect_use_graph_names(&plan.ops) {
        let name = ObjectName { parts };
        let focused = graph_context::resolve_graph_reference_for_plan(
            store,
            &name,
            caller,
            session_current,
            session_effective,
        )?;
        if focused != session_effective {
            return Err(RouterError::InvalidArgument(format!(
                "multi-graph USE GRAPH is not supported (focused graph `{}` differs from effective graph)",
                name.parts.join(".")
            )));
        }
    }
    Ok(())
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
    fn nested_use_graph_in_inline_call_rejected() {
        let store = RouterStore::new();
        register_graph(&store, "tenant_a", true);
        register_graph(&store, "tenant_b", false);
        let caller = Principal::anonymous();
        let query = "USE tenant_a { USE tenant_b { MATCH (n) RETURN n } }";
        let (program, block) = block_for_query(query);
        let effective = graph_context::resolve_graph_context(&store, &program, caller)
            .expect("resolve")
            .graph_id;
        let err = resolve_ingress_dispatch(&store, &program, &block, caller, effective)
            .expect_err("expected nested USE rejection");
        assert!(
            err.to_string().contains("multi-graph USE GRAPH")
                || err.to_string().contains("nested USE GRAPH"),
            "unexpected error: {err}"
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
}
