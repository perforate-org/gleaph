//! Prepared query catalog on the router (plan wire blobs).

use ic_cdk::api::msg_caller;

use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_gql_planner::build_block_plan_with_schema;
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::{GqlExecutionMode, GqlQueryResult};

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

pub fn prepared_register(name: String, query: String) -> Result<(), RouterError> {
    authorize_prepared_catalog_change(&msg_caller())?;
    let program = parser::parse(&query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let caller = msg_caller();
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
    let plan = build_block_plan_with_schema(&dispatch.plan_block, Some(&stats), schema)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let requires_write_path = plan.has_dml();
    let classified = classify_program(&program).requires_write_path();
    if requires_write_path != classified {
        return Err(RouterError::InvalidArgument(
            "planner DML content does not match program classification".into(),
        ));
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
    )
    .await?
    .row_count)
}

pub async fn prepared_execute_update_idempotent(
    name: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<GqlQueryResult, RouterError> {
    prepared_execute(
        name,
        params,
        GqlExecutionMode::Update,
        "prepared_execute_update_idempotent",
        false,
        Some(&client_mutation_key),
    )
    .await
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
    )
    .await?
    .row_count)
}

async fn prepared_execute(
    name: String,
    params: Vec<u8>,
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
    client_mutation_key: Option<&str>,
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
    let pmap =
        decode_gql_params_blob(&params).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let (_, plans) = gleaph_gql_planner::wire::decode_plan_bundle(&v1.plan_blob)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let stats = graph_stats_for(graph_id);
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
    caller: candid::Principal,
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
}
