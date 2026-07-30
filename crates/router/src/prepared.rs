//! Prepared query catalog on the router (plan wire blobs).

use candid::Principal;
use ic_cdk::api::msg_caller;
use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::gql::build_router_block_plan;
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::{GqlExecutionMode, GqlQueryResult, ReadMode};
use gleaph_prepared_api::{
    GraphIdentity, MANIFEST_VERSION, OperationKind, PreparedManifest, PreparedOperation,
};

use crate::execution_path::check_prepared_execution_path;
use crate::facade::stable::ROUTER_PREPARED_PLANS;
use crate::facade::stable::graph_catalog;
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

const POST_UPGRADE_PREPARED_CACHE_LIMIT: usize = 32;

#[derive(Clone)]
struct PreparedQueryCache {
    _program: gleaph_gql::ast::GqlProgram,
    _comments: Vec<gleaph_gql::token::Comment>,
    plan: PhysicalPlan,
    plan_blob: Vec<u8>,
    requires_write_path: bool,
}

enum PreparedCacheEntry {
    Ready(Box<PreparedQueryCache>),
    Failed(String),
}

thread_local! {
    static PREPARED_QUERY_CACHE: RefCell<BTreeMap<PreparedPlanKey, PreparedCacheEntry>> =
        const { RefCell::new(BTreeMap::new()) };
}

/// Plan a prepared query through the production Router ingress planning seam.
///
/// This is the exact planning path used by `prepared_register` after authorization,
/// exposed without `msg_caller` so unit tests can drive it with an explicit principal.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "retained as the explicit test planning seam")
)]
pub(crate) fn plan_prepared_query(
    query: &str,
    caller: Principal,
) -> Result<(PhysicalPlan, GraphId, bool), RouterError> {
    let program = parser::parse(query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    plan_prepared_program(&program, caller, None)
}

fn plan_prepared_program(
    program: &gleaph_gql::ast::GqlProgram,
    caller: Principal,
    forced_graph_id: Option<GraphId>,
) -> Result<(PhysicalPlan, GraphId, bool), RouterError> {
    let store = RouterStore::new();
    let resolved = match forced_graph_id {
        Some(graph_id) => graph_context::ResolvedGraphContext { graph_id },
        None => graph_context::resolve_graph_context(&store, program, caller)?,
    };
    let seed = graph_context::session_graph_seed(&store, resolved, caller);
    gleaph_gql::validate::validate_with_seed(program, Some(&seed))
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
        program,
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
    let classified = classify_program(program).requires_write_path();
    if requires_write_path != classified {
        return Err(RouterError::InvalidArgument(
            "planner DML content does not match program classification".into(),
        ));
    }
    if requires_write_path && !plan.is_pure_insert() {
        crate::gql::enforce_multi_dml_bundle_gate(&store, dispatch.dispatch_graph_id, block)?;
    }
    let session_current = graph_context::session_current_after_activity(&store, program, caller)?;
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
    prepared_register_core(&name, &query, caller, None)
}

/// Register a prepared operation together with its graph-scoped metadata.
pub fn prepared_register_with_metadata(
    name: String,
    query: String,
    metadata: PreparedOperation,
) -> Result<(), RouterError> {
    authorize_prepared_catalog_change(&msg_caller())?;
    let caller = msg_caller();
    prepared_register_core(&name, &query, caller, Some(metadata))
}

/// Batch variant of [`prepared_register`]. All items are authorized by the same caller and
/// registered independently; failures are reported per item so one duplicate does not abort the
/// whole social-demo scenario catalog.
pub fn prepared_register_batch(queries: Vec<(String, String)>) -> Vec<Result<(), RouterError>> {
    let caller = msg_caller();
    if let Err(e) = authorize_prepared_catalog_change(&caller) {
        return queries.into_iter().map(|_| Err(e.clone())).collect();
    }
    queries
        .into_iter()
        .map(|(name, query)| prepared_register_core(&name, &query, caller, None))
        .collect()
}

/// Batch registration variant carrying one metadata record per prepared operation.
pub fn prepared_register_with_metadata_batch(
    queries: Vec<(String, String, PreparedOperation)>,
) -> Vec<Result<(), RouterError>> {
    let caller = msg_caller();
    if let Err(e) = authorize_prepared_catalog_change(&caller) {
        return queries.into_iter().map(|_| Err(e.clone())).collect();
    }
    queries
        .into_iter()
        .map(|(name, query, metadata)| {
            prepared_register_core(&name, &query, caller, Some(metadata))
        })
        .collect()
}

fn prepared_register_core(
    name: &str,
    query: &str,
    caller: Principal,
    metadata: Option<PreparedOperation>,
) -> Result<(), RouterError> {
    let (cache, graph_id) = build_prepared_cache(query, caller, None)?;
    let requires_write_path = cache.requires_write_path;
    let mut metadata = metadata;
    if let Some(metadata) = &mut metadata {
        crate::prepared_documentation::apply_to_operation(&cache._program.doc_comments, metadata);
        crate::prepared_documentation::validate_typed_parameters(&cache._program, metadata)
            .map_err(RouterError::InvalidArgument)?;
    }
    if let Some(metadata) = &metadata {
        let expected_kind = if requires_write_path {
            OperationKind::Update
        } else {
            OperationKind::Query
        };
        if metadata.name != name {
            return Err(RouterError::InvalidArgument(format!(
                "prepared metadata name {:?} does not match registration name {:?}",
                metadata.name, name
            )));
        }
        if metadata.kind != expected_kind {
            return Err(RouterError::InvalidArgument(format!(
                "prepared metadata kind for {:?} does not match the planned execution path",
                name
            )));
        }
    }
    let key = prepared_key(graph_id, name);
    insert_prepared_plan(
        key,
        PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
            query: query.to_owned(),
            metadata,
        }),
    );
    insert_prepared_cache(
        prepared_key(graph_id, name),
        PreparedCacheEntry::Ready(Box::new(cache)),
    );
    Ok(())
}

fn build_prepared_cache(
    query: &str,
    caller: Principal,
    forced_graph_id: Option<GraphId>,
) -> Result<(PreparedQueryCache, GraphId), RouterError> {
    let parsed = parser::parse_with_comments(query)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let (plan, graph_id, requires_write_path) =
        plan_prepared_program(&parsed.program, caller, forced_graph_id)?;
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), requires_write_path)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    Ok((
        PreparedQueryCache {
            _program: parsed.program,
            _comments: parsed.comments,
            plan,
            plan_blob,
            requires_write_path,
        },
        graph_id,
    ))
}

fn insert_prepared_cache(key: PreparedPlanKey, entry: PreparedCacheEntry) {
    PREPARED_QUERY_CACHE.with_borrow_mut(|cache| {
        cache.insert(key, entry);
    });
}

fn cached_prepared_cache(key: &PreparedPlanKey) -> Option<PreparedQueryCache> {
    PREPARED_QUERY_CACHE.with_borrow(|cache| match cache.get(key) {
        Some(PreparedCacheEntry::Ready(entry)) => Some((**entry).clone()),
        Some(PreparedCacheEntry::Failed(reason)) => {
            let _ = reason;
            None
        }
        None => None,
    })
}

fn prepare_cache_for_execution(
    key: &PreparedPlanKey,
    graph_id: GraphId,
    record: &PreparedPlanRecordV1,
    caller: Principal,
) -> Result<PreparedQueryCache, RouterError> {
    if let Some(cache) = cached_prepared_cache(key) {
        return Ok(cache);
    }
    match build_prepared_cache(&record.query, caller, Some(graph_id)) {
        Ok((cache, planned_graph_id)) if planned_graph_id == graph_id => {
            insert_prepared_cache(
                key.clone(),
                PreparedCacheEntry::Ready(Box::new(cache.clone())),
            );
            Ok(cache)
        }
        Ok((_, planned_graph_id)) => Err(RouterError::InvalidArgument(format!(
            "prepared query resolved to graph {planned_graph_id:?}, expected {graph_id:?}"
        ))),
        Err(error) => {
            insert_prepared_cache(key.clone(), PreparedCacheEntry::Failed(error.to_string()));
            Err(error)
        }
    }
}

/// Best-effort prepared cache warmup after canister upgrade.
///
/// Stable query records remain authoritative. A failed parse or plan is recorded in heap state and
/// does not abort the upgrade; the next execution retries the failed entry lazily. The bounded
/// pass prevents a large catalog from exhausting the post-upgrade instruction budget.
pub(crate) fn rebuild_prepared_caches_after_upgrade() {
    PREPARED_QUERY_CACHE.with_borrow_mut(|cache| cache.clear());
    let records = ROUTER_PREPARED_PLANS.with_borrow(|plans| {
        plans
            .iter()
            .take(POST_UPGRADE_PREPARED_CACHE_LIMIT)
            .filter_map(|entry| Some((entry.key().clone(), entry.value().as_v1().ok()?.clone())))
            .collect::<Vec<_>>()
    });

    for (key, record) in records {
        let caller = graph_catalog::graph_entry(key.graph_id)
            .map(|entry| entry.owner)
            .unwrap_or_else(Principal::anonymous);
        match build_prepared_cache(&record.query, caller, Some(key.graph_id)) {
            Ok((cache, graph_id)) if graph_id == key.graph_id => {
                insert_prepared_cache(key, PreparedCacheEntry::Ready(Box::new(cache)));
            }
            Ok((_, graph_id)) => {
                insert_prepared_cache(
                    key,
                    PreparedCacheEntry::Failed(format!(
                        "prepared query resolved to graph {graph_id:?} during upgrade"
                    )),
                );
            }
            Err(error) => {
                insert_prepared_cache(key, PreparedCacheEntry::Failed(error.to_string()));
            }
        }
    }
}

/// Return the metadata snapshot for one authorized logical graph.
pub fn prepared_manifest(graph_name: String) -> Result<PreparedManifest, RouterError> {
    authorize_prepared_execute(&msg_caller())?;
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&graph_name, caller)?;
    let mut operations = ROUTER_PREPARED_PLANS.with_borrow(|plans| {
        plans
            .iter()
            .filter(|entry| entry.key().graph_id == graph_id)
            .filter_map(|entry| entry.value().as_v1().ok()?.metadata.clone())
            .collect::<Vec<_>>()
    });
    operations.sort_by(|left, right| left.name.cmp(&right.name));
    if operations.is_empty() {
        return Err(RouterError::NotFound(format!(
            "prepared metadata for graph {graph_name:?}"
        )));
    }
    Ok(PreparedManifest {
        manifest_version: MANIFEST_VERSION,
        graph: GraphIdentity {
            id: graph_name,
            name: None,
        },
        operations,
    })
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
    let result = prepared_execute_unchecked(
        name,
        params,
        mode,
        entrypoint,
        force,
        client_mutation_key,
        read_mode,
    )
    .await?;
    crate::gql::ensure_gql_query_result_payload(&result, entrypoint)?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn prepared_execute_unchecked(
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
    let cache = prepare_cache_for_execution(&key, graph_id, v1, caller)?;
    check_prepared_execution_path(entrypoint, mode, cache.requires_write_path, force)?;
    crate::gql::enforce_read_consistency(&store, graph_id, &read_mode).await?;
    let pmap =
        decode_gql_params_blob(&params).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let plans = vec![cache.plan.clone()];
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
        &cache.plan_blob,
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
