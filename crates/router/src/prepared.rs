//! Prepared query catalog on the router (plan wire blobs).

use ic_cdk::api::msg_caller;

use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_gql_planner::build_block_plan_with_schema;
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::plan_exec::GqlExecutionMode;

use crate::execution_path::check_prepared_execution_path;
use crate::facade::stable::ROUTER_PREPARED_PLANS;
use crate::gql::dispatch_plan_blob;
use crate::planner_stats::RouterGraphStats;
use crate::rbac::{authorize_prepared_catalog_change, authorize_prepared_execute};
use crate::state::RouterError;

#[derive(Clone, Debug)]
pub struct PreparedPlanRecord {
    pub plan_blob: Vec<u8>,
    pub requires_write_path: bool,
}

pub fn prepared_register(
    logical_graph_name: String,
    name: String,
    query: String,
) -> Result<(), RouterError> {
    authorize_prepared_catalog_change(&msg_caller())?;
    let program = parser::parse(&query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing transaction".into()))?;
    let block = tx
        .body
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing statement block".into()))?;
    let stats = RouterGraphStats::for_graph(&logical_graph_name);
    let plan = build_block_plan_with_schema(block, Some(&stats), &NoSchema)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let requires_write_path = plan.has_dml();
    let classified = classify_program(&program).requires_write_path();
    if requires_write_path != classified {
        return Err(RouterError::InvalidArgument(
            "planner DML content does not match program classification".into(),
        ));
    }
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), requires_write_path)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let key = prepared_key(&logical_graph_name, &name);
    ROUTER_PREPARED_PLANS.with_borrow_mut(|m| {
        m.insert(
            key,
            PreparedPlanRecord {
                plan_blob,
                requires_write_path,
            },
        );
    });
    Ok(())
}

pub fn prepared_drop(logical_graph_name: &str, name: &str) -> Result<(), RouterError> {
    authorize_prepared_catalog_change(&msg_caller())?;
    let key = prepared_key(logical_graph_name, name);
    ROUTER_PREPARED_PLANS.with_borrow_mut(|m| {
        m.remove(&key);
    });
    Ok(())
}

pub async fn prepared_execute_query(
    logical_graph_name: String,
    name: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    prepared_execute(
        logical_graph_name,
        name,
        params,
        GqlExecutionMode::Query,
        "prepared_execute_query",
        false,
    )
    .await
}

pub async fn prepared_execute_update(
    logical_graph_name: String,
    name: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    prepared_execute(
        logical_graph_name,
        name,
        params,
        GqlExecutionMode::Update,
        "prepared_execute_update",
        false,
    )
    .await
}

/// Run a read-only prepared plan on the **update** path (escape hatch only).
pub async fn force_prepared_execute_update(
    logical_graph_name: String,
    name: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    prepared_execute(
        logical_graph_name,
        name,
        params,
        GqlExecutionMode::Update,
        "force_prepared_execute_update",
        true,
    )
    .await
}

async fn prepared_execute(
    logical_graph_name: String,
    name: String,
    params: Vec<u8>,
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
) -> Result<u64, RouterError> {
    authorize_prepared_execute(&msg_caller())?;
    let key = prepared_key(&logical_graph_name, &name);
    let record = ROUTER_PREPARED_PLANS
        .with_borrow(|m| m.get(&key).cloned())
        .ok_or_else(|| RouterError::NotFound(format!("prepared query {name:?}")))?;
    check_prepared_execution_path(entrypoint, mode, record.requires_write_path, force)?;
    let pmap =
        decode_gql_params_blob(&params).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let (_, plans) = gleaph_gql_planner::wire::decode_plan_bundle(&record.plan_blob)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    dispatch_plan_blob(
        &logical_graph_name,
        &record.plan_blob,
        &plans,
        &pmap,
        &params,
        mode,
    )
    .await
}

fn prepared_key(logical_graph_name: &str, name: &str) -> String {
    format!("{logical_graph_name}\0{name}")
}
