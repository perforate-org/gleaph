//! Prepared query catalog on the router (plan wire blobs).

use candid::Principal;
use ic_cdk::api::msg_caller;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use crate::gql::build_router_block_plan;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::{GqlExecutionMode, GqlQueryResult, ReadMode};
use gleaph_prepared_api::{
    GraphIdentity, MANIFEST_VERSION, OperationKind, PreparedManifest, PreparedOperation,
    PreparedOperationRecord, PreparedRegistration, PreparedSortSpec,
};
use gleaph_prepared_runtime::parse_prepared_source;

use crate::execution_path::check_prepared_execution_path;
use crate::facade::stable::ROUTER_PREPARED_PLANS;
use crate::facade::stable::graph_catalog;
use crate::facade::stable::prepared_catalog::{
    PreparedPlanKey, PreparedPlanRecord, PreparedPlanRecordV1, get_prepared_plan,
    insert_prepared_plan, remove_prepared_plan,
};
use crate::facade::store::RouterStore;
use crate::gql::dispatch_plan_blob;
use crate::graph_context;
use crate::index_catalog::graph_stats_for;
use crate::rbac::authorize_prepared_catalog_change;
use crate::state::RouterError;
use crate::vector_sync;

const POST_UPGRADE_PREPARED_CACHE_LIMIT: usize = 32;

/// Maximum number of operations accepted by one [`prepare`] batch (ADR 0061).
pub const MAX_PREPARED_BATCH: usize = 32;

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
/// This is the exact planning path used by prepared registration after authorization,
/// exposed without `msg_caller` so unit tests can drive it with an explicit principal.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "retained as the explicit test planning seam")
)]
pub(crate) fn plan_prepared_query(
    query: &str,
    caller: Principal,
) -> Result<(PhysicalPlan, GraphId, bool), RouterError> {
    let parsed =
        parse_prepared_source(query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    plan_prepared_program(&parsed.program, caller, None)
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

/// Register or replace named prepared operations in one atomic batch (idempotent upsert).
///
/// The batch is all-or-nothing: every operation is parsed, planned, and metadata-completed
/// before any stable record or heap-cache entry is written; a single failure leaves the catalog
/// unchanged and reports the first failing operation in batch order (ADR 0061).
pub fn prepare(operations: Vec<PreparedRegistration>) -> Result<(), RouterError> {
    authorize_prepared_catalog_change(&msg_caller())?;
    let caller = msg_caller();
    prepare_batch_core(&operations, caller)
}

pub(crate) fn prepare_batch_core(
    operations: &[PreparedRegistration],
    caller: Principal,
) -> Result<(), RouterError> {
    if operations.len() > MAX_PREPARED_BATCH {
        return Err(RouterError::InvalidArgument(format!(
            "prepared batch exceeds the limit of {MAX_PREPARED_BATCH} operations"
        )));
    }
    let mut names = BTreeSet::new();
    for operation in operations {
        if !names.insert(operation.name.as_str()) {
            return Err(RouterError::InvalidArgument(format!(
                "duplicate prepared operation name {:?} in batch",
                operation.name
            )));
        }
    }
    // Phase 1: plan and complete metadata for every operation without writing. The first
    // failure in batch order aborts the whole batch before any catalog mutation.
    let mut committed = Vec::with_capacity(operations.len());
    for operation in operations {
        let (cache, graph_id) = build_prepared_cache(&operation.query, caller, None)
            .map_err(|error| batch_op_error(&operation.name, error))?;
        let mut metadata = operation.metadata.clone();
        if let Some(metadata) = &mut metadata {
            let open = NoSchema;
            let mut typed = None;
            let schema = crate::facade::stable::graph_type_catalog::property_schema_for_planning(
                graph_id, &open, &mut typed,
            )
            .map_err(|error| batch_op_error(&operation.name, error))?;
            crate::prepared_documentation::complete_parameter_metadata(
                &cache._program,
                schema,
                metadata,
            )
            .map_err(|error| {
                batch_op_error(&operation.name, RouterError::InvalidArgument(error))
            })?;
            crate::prepared_documentation::complete_result_schema(
                &cache._program,
                schema,
                metadata,
            )
            .map_err(|error| {
                batch_op_error(&operation.name, RouterError::InvalidArgument(error))
            })?;
            crate::prepared_documentation::apply_to_operation(
                &cache._program.doc_comments,
                metadata,
            );
        }
        validate_prepared_metadata(&operation.name, cache.requires_write_path, &metadata)
            .map_err(|error| batch_op_error(&operation.name, error))?;
        committed.push((
            PreparedPlanKey::new(&operation.name),
            PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
                graph_id,
                query: operation.query.clone(),
                metadata,
            }),
            cache,
        ));
    }
    // Phase 2: commit every stable record and heap-cache entry only after all operations planned.
    for (key, record, cache) in committed {
        insert_prepared_plan(key.clone(), record);
        insert_prepared_cache(key, PreparedCacheEntry::Ready(Box::new(cache)));
    }
    Ok(())
}

fn batch_op_error(name: &str, error: RouterError) -> RouterError {
    RouterError::InvalidArgument(format!("prepared op '{name}': {error}"))
}

fn validate_prepared_metadata(
    name: &str,
    requires_write_path: bool,
    metadata: &Option<PreparedOperation>,
) -> Result<(), RouterError> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    let expected_kind = if requires_write_path {
        OperationKind::Update
    } else {
        OperationKind::Query
    };
    if metadata.name != name {
        return Err(RouterError::InvalidArgument(format!(
            "prepared metadata name {:?} does not match upsert name {:?}",
            metadata.name, name
        )));
    }
    if metadata.kind != expected_kind {
        return Err(RouterError::InvalidArgument(format!(
            "prepared metadata kind for {:?} does not match the planned execution path",
            name
        )));
    }
    Ok(())
}

fn build_prepared_cache(
    query: &str,
    caller: Principal,
    forced_graph_id: Option<GraphId>,
) -> Result<(PreparedQueryCache, GraphId), RouterError> {
    let parsed =
        parse_prepared_source(query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
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
        let caller = graph_catalog::graph_entry(record.graph_id)
            .map(|entry| entry.owner)
            .unwrap_or_else(Principal::anonymous);
        match build_prepared_cache(&record.query, caller, Some(record.graph_id)) {
            Ok((cache, graph_id)) if graph_id == record.graph_id => {
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

/// Return the metadata snapshot for one logical graph (operator / codegen surface).
pub fn list_prepared(graph_name: Option<String>) -> Result<PreparedManifest, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let canonical_name = graph_catalog::graph_name(graph_id);
    let mut operations = ROUTER_PREPARED_PLANS.with_borrow(|plans| {
        plans
            .iter()
            .filter(|entry| entry.value().as_v1().is_ok_and(|v| v.graph_id == graph_id))
            .filter_map(|entry| entry.value().as_v1().ok()?.metadata.clone())
            .collect::<Vec<_>>()
    });
    operations.sort_by(|left, right| left.name.cmp(&right.name));
    if operations.is_empty() {
        return Err(RouterError::NotFound(format!(
            "prepared metadata for graph {canonical_name:?}"
        )));
    }
    Ok(PreparedManifest {
        manifest_version: MANIFEST_VERSION,
        graph: GraphIdentity {
            id: canonical_name.unwrap_or_default(),
            name: None,
        },
        operations,
    })
}

pub fn drop_prepared(name: &str) -> Result<(), RouterError> {
    authorize_prepared_catalog_change(&msg_caller())?;
    let key = PreparedPlanKey::new(name);
    if get_prepared_plan(&key).is_none() {
        return Err(RouterError::NotFound(format!("prepared query {name:?}")));
    }
    remove_prepared_plan(&key);
    Ok(())
}

/// Return the stored source and metadata of one registered prepared operation.
///
/// Lookup is by the Router-global operation name (ADR 0063); the bound graph is an
/// implementation detail of the record and is never exposed to the caller.
pub fn get_prepared(name: &str) -> Result<PreparedOperationRecord, RouterError> {
    get_prepared_core(name)
}

/// Read-back for [`get_prepared`], exposed separately so unit tests can drive it directly.
pub(crate) fn get_prepared_core(name: &str) -> Result<PreparedOperationRecord, RouterError> {
    let record = get_prepared_plan(&PreparedPlanKey::new(name))
        .ok_or_else(|| RouterError::NotFound(format!("prepared query {name:?}")))?;
    let v1 = record.as_v1()?;
    Ok(PreparedOperationRecord {
        query: v1.query.clone(),
        metadata: v1.metadata.clone(),
    })
}

/// Read-only prepared execution with an explicit ADR 0029 §5 consistency contract (Phase 3).
/// `ReadMode::Eventual` is the default contract; `ReadMode::AtLeast(token)` enforces the
/// read-your-writes barrier.
pub async fn prepared_query(
    name: String,
    params: Vec<u8>,
    sort: Option<Vec<PreparedSortSpec>>,
    read_mode: ReadMode,
) -> Result<GqlQueryResult, RouterError> {
    prepared_run(
        name,
        params,
        GqlExecutionMode::Query,
        "prepared_query",
        false,
        None,
        sort.as_deref(),
        read_mode,
    )
    .await
}

pub async fn prepared_mutate(
    name: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<GqlQueryResult, RouterError> {
    let result = prepared_run(
        name,
        params,
        GqlExecutionMode::Update,
        "prepared_mutate",
        false,
        Some(&client_mutation_key),
        None,
        ReadMode::Eventual,
    )
    .await;
    // ADR 0029 Phase 4: arm the recovery driver so a saga left non-terminal (canonical
    // committed, projection incomplete) converges without a client retry.
    crate::recovery::arm_if_needed();
    result
}

#[allow(clippy::too_many_arguments)]
async fn prepared_run(
    name: String,
    params: Vec<u8>,
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
    client_mutation_key: Option<&str>,
    sort: Option<&[PreparedSortSpec]>,
    read_mode: ReadMode,
) -> Result<GqlQueryResult, RouterError> {
    let result = prepared_run_unchecked(
        name,
        params,
        mode,
        entrypoint,
        force,
        client_mutation_key,
        sort,
        read_mode,
    )
    .await?;
    crate::gql::ensure_gql_query_result_payload(&result, entrypoint)?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn prepared_run_unchecked(
    name: String,
    params: Vec<u8>,
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
    client_mutation_key: Option<&str>,
    sort: Option<&[PreparedSortSpec]>,
    read_mode: ReadMode,
) -> Result<GqlQueryResult, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let key = PreparedPlanKey::new(&name);
    let record = get_prepared_plan(&key)
        .ok_or_else(|| RouterError::NotFound(format!("prepared query {name:?}")))?;
    let v1 = record.as_v1()?;
    let graph_id = v1.graph_id;
    let cache = prepare_cache_for_execution(&key, graph_id, v1, caller)?;
    let cache = match sort {
        Some(sort) if !sort.is_empty() => {
            prepare_sorted_cache(&cache, v1.metadata.as_ref(), sort, caller, graph_id)?
        }
        _ => cache,
    };
    check_prepared_execution_path(entrypoint, mode, cache.requires_write_path, force)?;
    crate::gql::enforce_read_consistency(&store, graph_id, &read_mode).await?;
    let pmap =
        decode_gql_params_blob(&params).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let plans = vec![cache.plan.clone()];
    let stats = graph_stats_for(graph_id);
    // ADR 0034: prepared queries that contain a supported `SEARCH` shape are lowered through the
    // same Router vector-index path as ad-hoc `gql_query`. The plan is single-graph by the
    // prepared upsert contract.
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

    // ADR 0029 Phase 5: federated multi-DML bundles are rejected at upsert (the AST is
    // available there), so a prepared plan that reaches dispatch is never a federated multi-DML
    // bundle. The contract 1/2 multi-DML admission (anchored single-shard and roll-forward fan-out)
    // applies to ad-hoc execution only; prepared multi-DML on a federated graph stays rejected at
    // upsert, where the runtime shard count is not yet known.
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

fn prepare_sorted_cache(
    base: &PreparedQueryCache,
    metadata: Option<&PreparedOperation>,
    sort: &[PreparedSortSpec],
    caller: Principal,
    graph_id: GraphId,
) -> Result<PreparedQueryCache, RouterError> {
    let metadata = metadata.ok_or_else(|| {
        RouterError::InvalidArgument("prepared sort requires operation metadata".into())
    })?;
    if metadata.kind != OperationKind::Query {
        return Err(RouterError::InvalidArgument(
            "prepared sort is only supported for query operations".into(),
        ));
    }
    let allowed: std::collections::BTreeSet<&str> = metadata
        .allowed_sorts
        .iter()
        .map(|key| key.key.as_str())
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut items = Vec::with_capacity(sort.len());
    for spec in sort {
        if !allowed.contains(spec.key.as_str()) {
            return Err(RouterError::InvalidArgument(format!(
                "sort key {:?} is not allowed for prepared query {:?}",
                spec.key, metadata.name
            )));
        }
        if !seen.insert(spec.key.as_str()) {
            return Err(RouterError::InvalidArgument(format!(
                "duplicate prepared sort key {:?}",
                spec.key
            )));
        }
        let direction = match spec.direction.to_ascii_lowercase().as_str() {
            "asc" | "ascending" => gleaph_gql::ast::SortDirection::Asc,
            "desc" | "descending" => gleaph_gql::ast::SortDirection::Desc,
            _ => {
                return Err(RouterError::InvalidArgument(format!(
                    "invalid prepared sort direction {:?}",
                    spec.direction
                )));
            }
        };
        items.push(gleaph_gql::ast::SortItem {
            span: gleaph_gql::token::Span::DUMMY,
            expr: gleaph_gql::ast::Expr::var(&spec.key),
            direction: Some(direction),
            null_order: None,
        });
    }

    let mut program = base._program.clone();
    let tx = program
        .transaction_activity
        .as_mut()
        .ok_or_else(|| RouterError::InvalidArgument("prepared query has no transaction".into()))?;
    let body = tx.body.as_mut().ok_or_else(|| {
        RouterError::InvalidArgument("prepared query has no statement body".into())
    })?;
    let gleaph_gql::ast::Statement::Query(query) = &mut body.first else {
        return Err(RouterError::InvalidArgument(
            "prepared sort requires a query statement".into(),
        ));
    };
    let result = query.left.result.as_mut().ok_or_else(|| {
        RouterError::InvalidArgument("prepared sort requires a result statement".into())
    })?;
    let order_by = gleaph_gql::ast::OrderByClause {
        span: gleaph_gql::token::Span::DUMMY,
        items,
    };
    match result {
        gleaph_gql::ast::ResultStatement::Return(return_statement) => {
            let gleaph_gql::ast::ReturnBody::Items {
                order_by: existing, ..
            } = &mut return_statement.body
            else {
                return Err(RouterError::InvalidArgument(
                    "prepared sort requires named result columns".into(),
                ));
            };
            *existing = Some(order_by);
        }
        gleaph_gql::ast::ResultStatement::Select(select_statement) => {
            let existing = match &mut select_statement.body {
                gleaph_gql::ast::SelectBody::Items { order_by, .. }
                | gleaph_gql::ast::SelectBody::Star { order_by, .. } => order_by,
            };
            *existing = Some(order_by);
        }
        gleaph_gql::ast::ResultStatement::Finish => {
            return Err(RouterError::InvalidArgument(
                "prepared sort requires a result statement".into(),
            ));
        }
    }

    let (plan, planned_graph_id, requires_write_path) =
        plan_prepared_program(&program, caller, Some(graph_id))?;
    if planned_graph_id != graph_id {
        return Err(RouterError::InvalidArgument(
            "prepared sort changed the resolved graph".into(),
        ));
    }
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), requires_write_path)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    Ok(PreparedQueryCache {
        _program: program,
        _comments: base._comments.clone(),
        plan,
        plan_blob,
        requires_write_path,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PREPARED_BATCH, PreparedPlanRecord, PreparedPlanRecordV1, cached_prepared_cache,
        get_prepared_core, get_prepared_plan, insert_prepared_plan, prepare_batch_core,
        rebuild_prepared_caches_after_upgrade, remove_prepared_plan,
    };
    use crate::facade::stable::prepared_catalog::PreparedPlanKey;
    use crate::state::RouterError;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_prepared_api::{
        OperationKind, PreparedOperation, PreparedRegistration, ResultSchema, SemanticType,
    };

    #[test]
    fn prepared_key_is_router_global_by_name() {
        assert_eq!(PreparedPlanKey::new("q1").name, "q1");
        assert_ne!(PreparedPlanKey::new("q1"), PreparedPlanKey::new("q2"));
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

    #[test]
    fn upgrade_rebuilds_prepared_cache_from_source_with_doc_comments() {
        let store = crate::facade::store::RouterStore::new();
        let owner = Principal::from_slice(&[2; 29]);
        let graph_id = GraphId::from_raw(9);
        crate::facade::auth::grant_admins(&[owner]);
        store
            .admin_register_graph(
                owner,
                GraphRegistryEntry {
                    graph_id,
                    graph_name: "prepared-runtime-cache".to_owned(),
                    canister_id: Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
            )
            .expect("register graph");

        let key = PreparedPlanKey::new("doc-query");
        insert_prepared_plan(
            key.clone(),
            PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
                graph_id,
                query: "/// generated docs\nMATCH (n:PreparedRuntimeCache) RETURN n".into(),
                metadata: None,
            }),
        );

        rebuild_prepared_caches_after_upgrade();

        let cache = cached_prepared_cache(&key).expect("cache rebuilt");
        assert_eq!(cache._program.doc_comments[0].text, "generated docs");
        assert_eq!(cache._comments[0].text, " generated docs");
        assert!(!cache.requires_write_path);
    }

    #[test]
    fn upgrade_rebuild_skips_invalid_source_without_blocking_valid_cache() {
        let store = crate::facade::store::RouterStore::new();
        let owner = Principal::from_slice(&[3; 29]);
        let graph_id = GraphId::from_raw(10);
        crate::facade::auth::grant_admins(&[owner]);
        store
            .admin_register_graph(
                owner,
                GraphRegistryEntry {
                    graph_id,
                    graph_name: "prepared-runtime-invalid-source".to_owned(),
                    canister_id: Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
            )
            .expect("register graph");

        let valid_key = PreparedPlanKey::new("valid-query");
        let invalid_key = PreparedPlanKey::new("invalid-query");
        insert_prepared_plan(
            valid_key.clone(),
            PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
                graph_id,
                query: "MATCH (n:PreparedRuntimeValid) RETURN n".into(),
                metadata: None,
            }),
        );
        insert_prepared_plan(
            invalid_key.clone(),
            PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
                graph_id,
                query: "this is not a prepared query".into(),
                metadata: None,
            }),
        );

        rebuild_prepared_caches_after_upgrade();

        assert!(cached_prepared_cache(&valid_key).is_some());
        assert!(cached_prepared_cache(&invalid_key).is_none());

        remove_prepared_plan(&valid_key);
        remove_prepared_plan(&invalid_key);
    }

    fn home_graph(owner: Principal, graph_name: &str) -> GraphId {
        let store = crate::facade::store::RouterStore::new();
        crate::facade::auth::grant_admins(&[owner]);
        store
            .admin_register_graph(
                owner,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    graph_name: graph_name.to_owned(),
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
            .expect("register graph");
        // `admin_register_graph` interns the graph id from the name; resolve the canonical id.
        crate::facade::stable::graph_catalog::lookup_graph_id(graph_name)
            .expect("interned graph id")
    }

    fn registration(name: &str, query: &str) -> PreparedRegistration {
        PreparedRegistration {
            name: name.into(),
            query: query.into(),
            metadata: None,
        }
    }

    fn operation(name: &str) -> PreparedOperation {
        PreparedOperation {
            name: name.into(),
            description: None,
            kind: OperationKind::Query,
            parameters: vec![],
            result: ResultSchema { columns: vec![] },
            supports_consistency: false,
            supports_idempotency: false,
            allowed_sorts: vec![],
        }
    }

    #[test]
    fn batch_prepare_registers_all_operations() {
        let owner = Principal::from_slice(&[11; 29]);
        home_graph(owner, "batch-register");
        let operations = vec![
            registration("alpha", "MATCH (n) RETURN 'a' AS tag"),
            registration("beta", "MATCH (n) RETURN n"),
        ];
        prepare_batch_core(&operations, owner).expect("batch registers");
        let alpha = get_prepared_plan(&PreparedPlanKey::new("alpha")).expect("alpha stored");
        let alpha = alpha.as_v1().expect("v1");
        assert_eq!(alpha.query, "MATCH (n) RETURN 'a' AS tag");
        assert!(alpha.metadata.is_none());
        assert!(
            cached_prepared_cache(&PreparedPlanKey::new("alpha")).is_some(),
            "alpha heap cache must be inserted"
        );
        let beta = get_prepared_plan(&PreparedPlanKey::new("beta")).expect("beta stored");
        let beta = beta.as_v1().expect("v1");
        assert_eq!(beta.query, "MATCH (n) RETURN n");
        remove_prepared_plan(&PreparedPlanKey::new("alpha"));
        remove_prepared_plan(&PreparedPlanKey::new("beta"));
    }

    #[test]
    fn batch_prepare_is_atomic_when_one_operation_fails() {
        let owner = Principal::from_slice(&[12; 29]);
        home_graph(owner, "batch-atomic");
        let operations = vec![
            registration("alpha", "MATCH (n) RETURN 'a' AS tag"),
            registration("broken", "this is not a prepared query"),
        ];
        let error = prepare_batch_core(&operations, owner).expect_err("batch must fail");
        assert!(error.to_string().contains("prepared op 'broken'"));
        assert!(
            get_prepared_plan(&PreparedPlanKey::new("alpha")).is_none(),
            "a failed batch must not commit the valid prefix"
        );
        assert!(
            cached_prepared_cache(&PreparedPlanKey::new("alpha")).is_none(),
            "a failed batch must not cache the valid prefix"
        );
    }

    #[test]
    fn batch_prepare_rejects_oversized_batch() {
        let owner = Principal::from_slice(&[13; 29]);
        home_graph(owner, "batch-limit");
        let operations: Vec<_> = (0..=MAX_PREPARED_BATCH)
            .map(|i| registration(&format!("op-{i}"), "MATCH (n) RETURN 'x' AS tag"))
            .collect();
        let error = prepare_batch_core(&operations, owner).expect_err("batch must exceed limit");
        assert!(error.to_string().contains("exceeds the limit"));
    }

    #[test]
    fn batch_prepare_rejects_duplicate_names() {
        let owner = Principal::from_slice(&[14; 29]);
        home_graph(owner, "batch-duplicate");
        let operations = vec![
            registration("dup", "MATCH (n) RETURN 'x' AS tag"),
            registration("dup", "MATCH (n) RETURN n"),
        ];
        let error = prepare_batch_core(&operations, owner).expect_err("duplicate must fail");
        assert!(
            error
                .to_string()
                .contains("duplicate prepared operation name")
        );
    }

    #[test]
    fn batch_prepare_reports_the_first_failure_in_batch_order() {
        let owner = Principal::from_slice(&[19; 29]);
        home_graph(owner, "batch-first-failure");
        let operations = vec![
            registration("first-ok", "MATCH (n) RETURN 'a' AS tag"),
            registration("second-bad", "this is not a prepared query"),
            registration("third-bad", "also not a prepared query"),
        ];
        let error = prepare_batch_core(&operations, owner).expect_err("batch must fail");
        assert!(error.to_string().contains("prepared op 'second-bad'"));
        assert!(
            !error.to_string().contains("third-bad"),
            "iteration must stop at the first failure in batch order"
        );
        assert!(
            get_prepared_plan(&PreparedPlanKey::new("first-ok")).is_none(),
            "the valid prefix must not commit"
        );
    }

    #[test]
    fn batch_prepare_rejects_kind_mismatch() {
        let owner = Principal::from_slice(&[20; 29]);
        home_graph(owner, "batch-kind");
        // A read-only program registered with Update metadata must fail the planned-path check.
        let mut op = registration("read-only", "MATCH (n) RETURN 'x' AS tag");
        let mut metadata = operation("read-only");
        metadata.kind = OperationKind::Update;
        op.metadata = Some(metadata);
        let error = prepare_batch_core(&[op], owner).expect_err("kind mismatch must fail");
        assert!(error.to_string().contains("prepared op 'read-only'"));
        assert!(
            error
                .to_string()
                .contains("does not match the planned execution path")
        );
    }

    #[test]
    fn batch_prepare_with_no_operations_is_a_no_op() {
        let owner = Principal::from_slice(&[21; 29]);
        home_graph(owner, "batch-empty");
        prepare_batch_core(&[], owner).expect("empty batch is a no-op success");
    }

    #[test]
    fn batch_prepare_rejects_metadata_name_mismatch() {
        let owner = Principal::from_slice(&[15; 29]);
        home_graph(owner, "batch-meta-mismatch");
        let mut op = registration("real-name", "MATCH (n) RETURN 'x' AS tag");
        let mut metadata = operation("other-name");
        metadata.name = "other-name".into();
        op.metadata = Some(metadata);
        let error = prepare_batch_core(&[op], owner).expect_err("mismatch must fail");
        assert!(error.to_string().contains("prepared op 'real-name'"));
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn batch_prepare_completes_result_schema() {
        let owner = Principal::from_slice(&[16; 29]);
        home_graph(owner, "batch-result");
        let mut op = registration("scalar-op", "MATCH (n) RETURN 'ok' AS tag");
        op.metadata = Some(operation("scalar-op"));
        prepare_batch_core(&[op], owner).expect("batch registers");
        let stored = get_prepared_plan(&PreparedPlanKey::new("scalar-op")).expect("stored");
        let record = stored.as_v1().expect("v1");
        let columns = record
            .metadata
            .as_ref()
            .expect("completed metadata")
            .result
            .columns
            .clone();
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "tag");
        assert_eq!(columns[0].semantic_type, SemanticType::Text);
        remove_prepared_plan(&PreparedPlanKey::new("scalar-op"));
    }

    #[test]
    fn get_prepared_returns_stored_query_and_metadata() {
        let owner = Principal::from_slice(&[17; 29]);
        home_graph(owner, "get-prepared");
        let mut op = registration("fetch-me", "MATCH (n) RETURN 'ok' AS tag");
        op.metadata = Some(operation("fetch-me"));
        prepare_batch_core(&[op], owner).expect("register");
        let record = get_prepared_core("fetch-me").expect("get_prepared");
        assert_eq!(record.query, "MATCH (n) RETURN 'ok' AS tag");
        let metadata = record.metadata.expect("metadata");
        assert_eq!(metadata.name, "fetch-me");
        assert_eq!(metadata.result.columns.len(), 1);
        remove_prepared_plan(&PreparedPlanKey::new("fetch-me"));
    }

    #[test]
    fn get_prepared_returns_not_found_for_missing_name() {
        let owner = Principal::from_slice(&[18; 29]);
        home_graph(owner, "get-prepared-missing");
        let error = get_prepared_core("nope").expect_err("missing must fail");
        assert!(matches!(error, RouterError::NotFound(_)));
    }
}
