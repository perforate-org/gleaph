//! UseGraph ingress guard (ADR 0011 U1a).

use candid::Principal;
use gleaph_gql::ast::{GqlProgram, ObjectName};
use gleaph_gql_planner::{PhysicalPlan, PlanOp};
use gleaph_graph_kernel::entry::GraphId;

use crate::facade::store::RouterStore;
use crate::graph_context;
use crate::state::RouterError;

/// Reject plans whose `UseGraph` targets differ from `effective` (multi-graph U1b not implemented).
pub fn ensure_single_graph_plan(
    store: &RouterStore,
    program: &GqlProgram,
    plan: &PhysicalPlan,
    effective: GraphId,
    caller: Principal,
) -> Result<(), RouterError> {
    let session_current = graph_context::session_current_after_activity(store, program, caller)?;
    for parts in collect_use_graph_names(&plan.ops) {
        let name = ObjectName { parts };
        let focused = graph_context::resolve_graph_reference_for_plan(
            store,
            &name,
            caller,
            session_current,
            effective,
        )?;
        if focused != effective {
            return Err(RouterError::InvalidArgument(format!(
                "multi-graph UseGraph is not supported (focused graph `{}` differs from effective graph)",
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
    use gleaph_gql::type_check::NoSchema;
    use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
    use gleaph_gql_planner::build_block_plan_with_schema;

    use crate::facade::stable::graph_catalog;
    use crate::index_catalog::graph_stats_for;

    fn register_graph(store: &RouterStore, name: &str) {
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
                },
            )
            .expect("register");
    }

    fn plan_for_query(store: &RouterStore, query: &str, caller: Principal) -> PhysicalPlan {
        let program = parser::parse(query).expect("parse");
        let resolved =
            graph_context::resolve_graph_context(store, &program, caller).expect("resolve");
        let tx = program.transaction_activity.as_ref().expect("tx");
        let block = tx.body.as_ref().expect("block");
        let stats = graph_stats_for(resolved.graph_id);
        build_block_plan_with_schema(block, Some(&stats), &NoSchema).expect("plan")
    }

    #[test]
    fn matching_use_graph_passes_guard() {
        let store = RouterStore::new();
        register_graph(&store, "tenant_a");
        let caller = Principal::anonymous();
        let query = "SESSION SET GRAPH tenant_a USE tenant_a MATCH (n) RETURN n";
        let program = parser::parse(query).expect("parse");
        let resolved =
            graph_context::resolve_graph_context(&store, &program, caller).expect("resolve");
        let plan = plan_for_query(&store, query, caller);
        ensure_single_graph_plan(&store, &program, &plan, resolved.graph_id, caller).expect("ok");
    }

    #[test]
    fn mismatched_use_graph_rejected() {
        let store = RouterStore::new();
        register_graph(&store, "tenant_a");
        register_graph(&store, "tenant_b");
        let caller = Principal::anonymous();
        let query = "SESSION SET GRAPH tenant_a USE tenant_b MATCH (n) RETURN n";
        let program = parser::parse(query).expect("parse");
        let resolved =
            graph_context::resolve_graph_context(&store, &program, caller).expect("resolve");
        let plan = plan_for_query(&store, query, caller);
        let err = ensure_single_graph_plan(&store, &program, &plan, resolved.graph_id, caller)
            .expect_err("expected rejection");
        assert!(
            err.to_string().contains("multi-graph UseGraph"),
            "unexpected error: {err}"
        );
        assert_eq!(
            graph_catalog::lookup_graph_id("tenant_a"),
            Some(resolved.graph_id)
        );
    }
}
