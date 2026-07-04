//! Prepared query catalog on the router (plan wire blobs).

use candid::Principal;
use ic_cdk::api::msg_caller;

use crate::gql::build_router_block_plan;
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::{GqlExecutionMode, GqlQueryResult, ReadMode};

use crate::execution_path::check_prepared_execution_path;
use crate::facade::stable::prepared_catalog::{
    PreparedPlanKey, PreparedPlanRecord, PreparedPlanRecordV1, contains_prepared_plan,
    get_prepared_plan, insert_prepared_plan, remove_prepared_plan,
};
use crate::facade::store::RouterStore;
use crate::gql::dispatch_plan_blob;
use crate::graph_context;
use crate::index_catalog::graph_stats_for;
use crate::rbac::{authorize_prepared_catalog_change, authorize_prepared_execute};
use crate::state::RouterError;
use crate::vector_sync;

/// Plan a prepared query through the production Router ingress planning seam.
///
/// This is the exact planning path used by `prepared_register` after authorization,
/// exposed without `msg_caller` so unit tests can drive it with an explicit principal.
pub(crate) fn plan_prepared_query(
    query: &str,
    caller: Principal,
) -> Result<(PhysicalPlan, GraphId, bool), RouterError> {
    let program = parser::parse(query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let store = RouterStore::new();
    let resolved = graph_context::resolve_graph_context(&store, &program, caller)?;
    let seed = graph_context::session_graph_seed(&store, resolved, caller);
    gleaph_gql::validate::validate_with_seed(&program, Some(&seed))
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing transaction".into()))?;
    let block = tx
        .body
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing statement block".into()))?;
    let dispatch = crate::use_graph::resolve_ingress_dispatch(
        &store,
        &program,
        block,
        caller,
        resolved.graph_id,
    )?;
    crate::facade::stable::graph_type_catalog::validate_block_schema_for_graph(
        &dispatch.plan_block,
        &seed,
        dispatch.dispatch_graph_id,
    )?;
    let stats = graph_stats_for(dispatch.dispatch_graph_id);
    let open = NoSchema;
    let mut typed = None;
    let schema = crate::facade::stable::graph_type_catalog::property_schema_for_planning(
        dispatch.dispatch_graph_id,
        &open,
        &mut typed,
    )?;
    let plan = build_router_block_plan(&dispatch.plan_block, schema, &stats)?;
    let requires_write_path = plan.has_dml();
    let classified = classify_program(&program).requires_write_path();
    if requires_write_path != classified {
        return Err(RouterError::InvalidArgument(
            "planner DML content does not match program classification".into(),
        ));
    }
    if requires_write_path && !plan.is_pure_insert() {
        crate::gql::enforce_multi_dml_bundle_gate(&store, dispatch.dispatch_graph_id, block)?;
    }
    let session_current = graph_context::session_current_after_activity(&store, &program, caller)?;
    let (graph_id, plan) = match crate::use_graph::analyze_use_graph_v2_dispatch(
        plan,
        &store,
        caller,
        session_current,
        resolved.graph_id,
    )? {
        crate::use_graph::UseGraphV2Dispatch::EffectiveGraph { plan } => {
            (dispatch.dispatch_graph_id, plan)
        }
        crate::use_graph::UseGraphV2Dispatch::Single { graph_id, plan } => (graph_id, plan),
        crate::use_graph::UseGraphV2Dispatch::Multi { .. }
        | crate::use_graph::UseGraphV2Dispatch::Join { .. } => {
            return Err(RouterError::InvalidArgument(
                "prepared queries with multi-graph USE GRAPH merge are not supported".into(),
            ));
        }
    };
    Ok((plan, graph_id, requires_write_path))
}

pub fn prepared_register(name: String, query: String) -> Result<(), RouterError> {
    authorize_prepared_catalog_change(&msg_caller())?;
    let caller = msg_caller();
    let (plan, graph_id, requires_write_path) = plan_prepared_query(&query, caller)?;
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), requires_write_path)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let key = prepared_key(graph_id, &name);
    insert_prepared_plan(
        key,
        PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
            plan_blob,
            requires_write_path,
        }),
    );
    Ok(())
}

pub fn prepared_drop(name: &str) -> Result<(), RouterError> {
    authorize_prepared_catalog_change(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = resolve_prepared_graph_id(&store, msg_caller(), name)?;
    remove_prepared_plan(&prepared_key(graph_id, name));
    Ok(())
}

pub async fn prepared_execute_query(
    name: String,
    params: Vec<u8>,
) -> Result<GqlQueryResult, RouterError> {
    prepared_execute(
        name,
        params,
        GqlExecutionMode::Query,
        "prepared_execute_query",
        false,
        None,
        ReadMode::Eventual,
    )
    .await
}

/// Run a prepared read with an explicit ADR 0029 §5 consistency contract (Phase 3).
pub async fn prepared_execute_query_with_consistency(
    name: String,
    params: Vec<u8>,
    read_mode: ReadMode,
) -> Result<GqlQueryResult, RouterError> {
    prepared_execute(
        name,
        params,
        GqlExecutionMode::Query,
        "prepared_execute_query_with_consistency",
        false,
        None,
        read_mode,
    )
    .await
}

pub async fn prepared_execute_update(name: String, params: Vec<u8>) -> Result<u64, RouterError> {
    Ok(prepared_execute(
        name,
        params,
        GqlExecutionMode::Update,
        "prepared_execute_update",
        false,
        None,
        ReadMode::Eventual,
    )
    .await?
    .row_count)
}

pub async fn prepared_execute_update_idempotent(
    name: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<GqlQueryResult, RouterError> {
    let result = prepared_execute(
        name,
        params,
        GqlExecutionMode::Update,
        "prepared_execute_update_idempotent",
        false,
        Some(&client_mutation_key),
        ReadMode::Eventual,
    )
    .await;
    // ADR 0029 Phase 4: arm the recovery driver so a saga left non-terminal (canonical
    // committed, projection incomplete) converges without a client retry.
    crate::recovery::arm_if_needed();
    result
}

/// Run a read-only prepared plan on the **update** path (escape hatch only).
pub async fn force_prepared_execute_update(
    name: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    Ok(prepared_execute(
        name,
        params,
        GqlExecutionMode::Update,
        "force_prepared_execute_update",
        true,
        None,
        ReadMode::Eventual,
    )
    .await?
    .row_count)
}

#[allow(clippy::too_many_arguments)]
async fn prepared_execute(
    name: String,
    params: Vec<u8>,
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
    client_mutation_key: Option<&str>,
    read_mode: ReadMode,
) -> Result<GqlQueryResult, RouterError> {
    authorize_prepared_execute(&msg_caller())?;
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id = resolve_prepared_graph_id(&store, caller, &name)?;
    let key = prepared_key(graph_id, &name);
    let record = get_prepared_plan(&key)
        .ok_or_else(|| RouterError::NotFound(format!("prepared query {name:?}")))?;
    let v1 = record.as_v1()?;
    check_prepared_execution_path(entrypoint, mode, v1.requires_write_path, force)?;
    crate::gql::enforce_read_consistency(&store, graph_id, &read_mode).await?;
    let pmap =
        decode_gql_params_blob(&params).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let (_, plans) = gleaph_gql_planner::wire::decode_plan_bundle(&v1.plan_blob)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let stats = graph_stats_for(graph_id);
    // ADR 0034: prepared queries that contain a supported `SEARCH` shape are lowered through the
    // same Router vector-index path as ad-hoc `gql_query`. The plan is single-graph by the
    // prepared registration contract.
    //
    // Limitation: the SEARCH lowering currently executes at `ReadMode::Eventual`. Passing the
    // caller-supplied `read_mode` through to `try_execute_gql_search` would require extending the
    // vector-search branch to honor projection-lag barriers the same way non-search read dispatch
    // does. That is out of scope for this slice; social-demo semantic scenarios are read-only and
    // use the default eventual-read consistency.
    if mode == GqlExecutionMode::Query
        && let Some(plan) = plans.first()
        && gleaph_gql_planner::plan_contains_search(plan)
        && let Some(result) = crate::gql_search::try_execute_gql_search(
            plan,
            graph_id,
            &params,
            mode,
            &stats,
            &store,
            msg_caller(),
            |target, req| async move {
                vector_sync::vector_search(target, req)
                    .await
                    .map_err(crate::state::RouterError::Internal)
            },
        )
        .await?
    {
        return Ok(result);
    }

    // ADR 0029 Phase 5: federated multi-DML bundles are rejected at registration (the AST is
    // available there), so a prepared plan that reaches dispatch is never a federated multi-DML
    // bundle. The contract 1/2 multi-DML admission (anchored single-shard and roll-forward fan-out)
    // applies to ad-hoc execution only; prepared multi-DML on a federated graph stays rejected at
    // registration, where the runtime shard count is not yet known.
    dispatch_plan_blob(
        graph_id,
        &v1.plan_blob,
        &plans,
        &pmap,
        &params,
        mode,
        client_mutation_key,
        &stats,
    )
    .await
}

fn resolve_prepared_graph_id(
    store: &RouterStore,
    caller: Principal,
    name: &str,
) -> Result<GraphId, RouterError> {
    let visible = store.list_visible_graph_ids(caller)?;
    let mut matches = Vec::new();
    for graph_id in visible {
        if contains_prepared_plan(&prepared_key(graph_id, name)) {
            matches.push(graph_id);
        }
    }
    match matches.as_slice() {
        [only] => Ok(*only),
        [] => Err(RouterError::NotFound(format!("prepared query {name:?}"))),
        _ => Err(RouterError::InvalidArgument(format!(
            "prepared query {name:?} is ambiguous across visible graphs"
        ))),
    }
}

pub(crate) fn prepared_key(graph_id: GraphId, name: &str) -> PreparedPlanKey {
    PreparedPlanKey::new(graph_id, name)
}

#[cfg(test)]
mod tests {
    use super::prepared_key;
    use gleaph_graph_kernel::entry::GraphId;

    #[test]
    fn prepared_key_scopes_by_graph_and_name() {
        let graph = GraphId::from_raw(7);
        assert_eq!(prepared_key(graph, "q1").name, "q1");
        assert_eq!(prepared_key(graph, "q1").graph_id, graph);
        assert_ne!(prepared_key(graph, "q1"), prepared_key(graph, "q2"));
        assert_ne!(
            prepared_key(graph, "q1"),
            prepared_key(GraphId::from_raw(8), "q1")
        );
    }

    use candid::Principal;
    use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
    use gleaph_gql_planner::plan::{PlanOp, ShortestPathCost};

    #[test]
    fn prepared_block_plan_accepts_cost_by_inline_property() {
        let store = crate::facade::store::RouterStore::new();
        let owner = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[owner]);
        store
            .admin_register_graph(
                owner,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    graph_name: "g1".to_owned(),
                    canister_id: Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: true,
                },
            )
            .expect("register");
        let query = "MATCH ANY SHORTEST (a:CitySrc)-[e:ROAD]->{1,5}(c:CityDst) COST BY e.distance RETURN a, c";
        let plan = crate::prepared::plan_prepared_query(query, owner)
            .expect("prepared planning seam should accept COST BY")
            .0;
        let cost = plan
            .ops
            .iter()
            .find_map(|op| match op {
                PlanOp::ShortestPath { cost, .. } => Some(cost.clone()),
                _ => None,
            })
            .expect("ShortestPath cost");
        assert!(
            matches!(&cost, ShortestPathCost::EdgeCostExpr { edge_var, .. } if edge_var.as_ref() == "e"),
            "expected COST BY in prepared planning to lower to EdgeCostExpr, got {cost:?}"
        );
    }
}
