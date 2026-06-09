//! Router-side GQL parse, plan, index seed routing, and graph dispatch.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use candid::Principal;
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::build_block_plan_with_schema;
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};
use gleaph_graph_kernel::index::{IndexIntersectionRequest, PostingHit};
use gleaph_graph_kernel::plan_exec::{GqlExecutionMode, LabelTelemetryEventWire, MutationId};
use ic_cdk::api::msg_caller;

use crate::execution_path::check_adhoc_execution_path;
use crate::facade::stable::label_telemetry::RouterMutationShard;
use crate::facade::store::RouterStore;
use crate::federation::{
    FederatedMergeMode, ShardDispatch, ShardingPolicy, apply_federated_aggregate_having,
    empty_execute_plan_result, federated_dispatch_plan_blob, federated_merge_mode_from_plans,
    merge_execute_plan_result, routings_to_dispatches, sharding_policy_for,
};
use crate::graph_client::{
    ack_label_telemetry_event, execute_plan_on_graph, get_mutation_outcome,
    list_pending_label_telemetry_events,
};
use crate::index_client::RouterIndexClient;
use crate::planner_stats::RouterGraphStats;
use crate::rbac::authorize_adhoc_gql;
use crate::seed::IndexAnchor;
use crate::state::RouterError;

trait IndexLookup {
    fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>>;

    fn lookup_intersection(
        &self,
        req: IndexIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>>;
}

impl IndexLookup for RouterIndexClient {
    fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
        Box::pin(self.lookup_equal(property_id, value))
    }

    fn lookup_intersection(
        &self,
        req: IndexIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
        Box::pin(self.lookup_intersection(req))
    }
}

pub async fn gql_query(
    logical_graph_name: String,
    query: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    run_gql(
        &logical_graph_name,
        &query,
        &params,
        GqlExecutionMode::Query,
        "gql_query",
        false,
        None,
    )
    .await
}

pub async fn gql_execute(
    logical_graph_name: String,
    query: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    run_gql(
        &logical_graph_name,
        &query,
        &params,
        GqlExecutionMode::Update,
        "gql_execute",
        false,
        None,
    )
    .await
}

pub async fn gql_execute_idempotent(
    logical_graph_name: String,
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<u64, RouterError> {
    run_gql(
        &logical_graph_name,
        &query,
        &params,
        GqlExecutionMode::Update,
        "gql_execute_idempotent",
        false,
        Some(&client_mutation_key),
    )
    .await
}

/// Run a read-only program on the **update** path (higher cost; escape hatch only).
pub async fn force_gql_execute(
    logical_graph_name: String,
    query: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    run_gql(
        &logical_graph_name,
        &query,
        &params,
        GqlExecutionMode::Update,
        "force_gql_execute",
        true,
        None,
    )
    .await
}

async fn run_gql(
    logical_graph_name: &str,
    query: &str,
    params: &[u8],
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
    client_mutation_key: Option<&str>,
) -> Result<u64, RouterError> {
    let program = parser::parse(query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let flags = classify_program(&program);
    let caller = msg_caller();
    authorize_adhoc_gql(&caller, flags)?;
    check_adhoc_execution_path(entrypoint, mode, flags, force)?;

    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing transaction".into()))?;
    let block = tx
        .body
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing statement block".into()))?;

    let stats = RouterGraphStats::for_graph(logical_graph_name);
    let plan = build_block_plan_with_schema(block, Some(&stats), &NoSchema)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let requires_write_path = plan.has_dml();
    if requires_write_path != flags.requires_write_path() {
        return Err(RouterError::InvalidArgument(
            "planner DML content does not match program classification".into(),
        ));
    }
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), requires_write_path)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

    let pmap =
        decode_gql_params_blob(params).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    dispatch_plan_blob(
        logical_graph_name,
        &plan_blob,
        std::slice::from_ref(&plan),
        &pmap,
        params,
        mode,
        client_mutation_key,
    )
    .await
}

/// Route and execute a plan blob (single- or multi-shard).
pub async fn dispatch_plan_blob(
    logical_graph_name: &str,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
) -> Result<u64, RouterError> {
    let store = RouterStore::new();
    let shards = store.list_shards_for_graph(logical_graph_name)?;
    if shards.is_empty() {
        return Err(RouterError::ShardNotRegistered);
    }
    let index = RouterIndexClient::new(shards[0].index_canister);
    dispatch_plan_blob_with_index(
        logical_graph_name,
        plan_blob,
        plans,
        pmap,
        params,
        mode,
        client_mutation_key,
        &store,
        shards,
        &index,
        msg_caller(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_plan_blob_with_index<I: IndexLookup + ?Sized>(
    logical_graph_name: &str,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
    store: &RouterStore,
    shards: Vec<ShardRegistryEntry>,
    index: &I,
    caller: Principal,
) -> Result<u64, RouterError> {
    let has_dml = plans.iter().any(PhysicalPlan::has_dml);
    let merge_mode = federated_merge_mode_from_plans(plans);
    let dispatch_plan_blob = federated_dispatch_plan_blob(shards.len(), plan_blob, plans, has_dml)
        .map_err(RouterError::InvalidArgument)?;
    let mutation_reservation = if has_dml {
        let key = client_mutation_key.ok_or_else(|| {
            RouterError::InvalidArgument(
                "DML execution requires client_mutation_key; use the idempotent update entrypoint"
                    .into(),
            )
        })?;
        Some(store.reserve_mutation_id_for_client_key(
            caller,
            logical_graph_name,
            key,
            request_fingerprint(plan_blob, params, mode),
        )?)
    } else {
        None
    };
    let mutation_id = mutation_reservation.map(|reservation| reservation.mutation_id);

    if has_dml && let Some(key) = client_mutation_key {
        if let Some(row_count) =
            store.router_mutation_completed_row_count(caller, logical_graph_name, key)
        {
            return Ok(row_count);
        }
        reconcile_router_mutation_telemetry(&store, caller, logical_graph_name, key).await?;
        if let Some(row_count) =
            store.router_mutation_completed_row_count(caller, logical_graph_name, key)
        {
            return Ok(row_count);
        }
    }

    let saved_record = client_mutation_key
        .and_then(|key| store.router_mutation_record(caller, logical_graph_name, key));
    let mut resolved_labels = match saved_record
        .as_ref()
        .and_then(|record| record.resolved_labels.clone())
    {
        Some(resolved_labels) => resolved_labels,
        None => match store.resolve_plan_labels(plans) {
            Ok(resolved_labels) => resolved_labels,
            Err(err) => {
                release_routing_if_owner(
                    &store,
                    caller,
                    logical_graph_name,
                    client_mutation_key,
                    mutation_reservation,
                )?;
                return Err(err);
            }
        },
    };

    let mut dispatches: Vec<ShardDispatch> = if let Some(record) = saved_record.as_ref()
        && !record.shards.is_empty()
    {
        record
            .shards
            .iter()
            .map(|shard| ShardDispatch {
                shard_id: shard.shard_id,
                graph_canister: shard.graph_canister,
                seed_bindings_blob: shard.seed_bindings_blob.clone(),
            })
            .collect()
    } else {
        let index_anchor = match IndexAnchor::from_plans(plans, pmap, store) {
            Ok(index_anchor) => index_anchor,
            Err(err) => {
                release_routing_if_owner(
                    &store,
                    caller,
                    logical_graph_name,
                    client_mutation_key,
                    mutation_reservation,
                )?;
                return Err(err);
            }
        };
        let policy = sharding_policy_for(&shards);
        let routings = match index_anchor {
            Some(anchor) => {
                let hits = match &anchor {
                    IndexAnchor::Equal(probe) => match index
                        .lookup_equal(probe.property_id, probe.payload_bytes.clone())
                        .await
                        .map_err(RouterError::InvalidArgument)
                    {
                        Ok(hits) => hits,
                        Err(err) => {
                            release_routing_if_owner(
                                &store,
                                caller,
                                logical_graph_name,
                                client_mutation_key,
                                mutation_reservation,
                            )?;
                            return Err(err);
                        }
                    },
                    IndexAnchor::Intersection { specs, .. } => {
                        match index
                            .lookup_intersection(IndexIntersectionRequest {
                                specs: specs.clone(),
                            })
                            .await
                            .map_err(RouterError::InvalidArgument)
                        {
                            Ok(hits) => hits,
                            Err(err) => {
                                release_routing_if_owner(
                                    &store,
                                    caller,
                                    logical_graph_name,
                                    client_mutation_key,
                                    mutation_reservation,
                                )?;
                                return Err(err);
                            }
                        }
                    }
                };
                if hits.is_empty() {
                    if let Some(key) = client_mutation_key {
                        store.record_router_mutation_completed_without_shards(
                            caller,
                            logical_graph_name,
                            key,
                            resolved_labels.clone(),
                            0,
                        )?;
                    }
                    return Ok(0);
                }
                match policy.resolve_with_hits(store, logical_graph_name, &shards, anchor, &hits) {
                    Ok(routings) => routings,
                    Err(err) => {
                        release_routing_if_owner(
                            &store,
                            caller,
                            logical_graph_name,
                            client_mutation_key,
                            mutation_reservation,
                        )?;
                        return Err(err);
                    }
                }
            }
            None => match policy.resolve_without_anchor(&shards) {
                Ok(routings) => routings,
                Err(err) => {
                    release_routing_if_owner(
                        &store,
                        caller,
                        logical_graph_name,
                        client_mutation_key,
                        mutation_reservation,
                    )?;
                    return Err(err);
                }
            },
        };
        routings_to_dispatches(routings)
    };

    if let (Some(key), Some(_)) = (client_mutation_key, mutation_id)
        && mutation_reservation.is_some_and(|reservation| reservation.routing_owner)
    {
        let envelope_shards = dispatches
            .iter()
            .map(|dispatch| {
                RouterMutationShard::new(
                    dispatch.shard_id,
                    dispatch.graph_canister,
                    dispatch.seed_bindings_blob.clone(),
                )
            })
            .collect();
        store.record_router_mutation_shards(
            caller,
            logical_graph_name,
            key,
            resolved_labels.clone(),
            envelope_shards,
        )?;
        if let Some(record) = store.router_mutation_record(caller, logical_graph_name, key) {
            if let Some(saved_resolved_labels) = record.resolved_labels {
                resolved_labels = saved_resolved_labels;
            }
            dispatches = record
                .shards
                .into_iter()
                .map(|shard| ShardDispatch {
                    shard_id: shard.shard_id,
                    graph_canister: shard.graph_canister,
                    seed_bindings_blob: shard.seed_bindings_blob,
                })
                .collect();
        }
    }

    let mut merged = empty_execute_plan_result();
    for dispatch in dispatches {
        let result = match execute_plan_on_graph(
            dispatch.graph_canister,
            gleaph_graph_kernel::plan_exec::ExecutePlanArgs {
                target_shard_id: dispatch.shard_id,
                mutation_id,
                plan_blob: dispatch_plan_blob.clone(),
                params_blob: params.to_vec(),
                mode,
                seed_bindings_blob: dispatch.seed_bindings_blob.clone(),
                resolved_labels: Some(resolved_labels.clone()),
            },
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                if let Some(mutation_id) = mutation_id {
                    if let Some(outcome) = recover_mutation_outcome(
                        &store,
                        dispatch.graph_canister,
                        dispatch.shard_id,
                        mutation_id,
                    )
                    .await?
                    {
                        if outcome.completed {
                            let events = outcome.label_telemetry_events.clone();
                            merge_execute_plan_result(
                                &mut merged,
                                gleaph_graph_kernel::plan_exec::ExecutePlanResult {
                                    row_count: outcome.row_count,
                                    label_telemetry_events: events.clone(),
                                    rows_blob: None,
                                },
                                merge_mode.clone(),
                            )
                            .map_err(RouterError::InvalidArgument)?;
                            if let Some(key) = client_mutation_key {
                                store.record_router_mutation_shard_completed(
                                    caller,
                                    logical_graph_name,
                                    key,
                                    dispatch.shard_id,
                                    outcome.row_count,
                                    events,
                                )?;
                                store.record_router_mutation_shard_telemetry_acked(
                                    caller,
                                    logical_graph_name,
                                    key,
                                    dispatch.shard_id,
                                )?;
                            }
                            continue;
                        }
                    }
                }
                return Err(RouterError::InvalidArgument(err));
            }
        };
        let telemetry_events = result.label_telemetry_events.clone();
        let telemetry_acked = apply_and_ack_label_telemetry_events(
            &store,
            dispatch.graph_canister,
            dispatch.shard_id,
            mutation_id,
            &telemetry_events,
        )
        .await?;
        if let Some(key) = client_mutation_key {
            store.record_router_mutation_shard_completed(
                caller,
                logical_graph_name,
                key,
                dispatch.shard_id,
                result.row_count,
                telemetry_events,
            )?;
            if telemetry_acked {
                store.record_router_mutation_shard_telemetry_acked(
                    caller,
                    logical_graph_name,
                    key,
                    dispatch.shard_id,
                )?;
            }
        }
        merge_execute_plan_result(&mut merged, result, merge_mode.clone())
            .map_err(RouterError::InvalidArgument)?;
    }
    if let FederatedMergeMode::Aggregate(spec) = &merge_mode {
        apply_federated_aggregate_having(&mut merged, spec, pmap)
            .map_err(RouterError::InvalidArgument)?;
    }
    if let Some(key) = client_mutation_key
        && let Some(row_count) =
            store.router_mutation_completed_row_count(caller, logical_graph_name, key)
    {
        return Ok(row_count);
    }
    Ok(merged.row_count)
}

fn release_routing_if_owner(
    store: &RouterStore,
    caller: Principal,
    logical_graph_name: &str,
    client_mutation_key: Option<&str>,
    mutation_reservation: Option<crate::facade::store::ClientMutationReservation>,
) -> Result<(), RouterError> {
    if let (Some(key), Some(reservation)) = (client_mutation_key, mutation_reservation)
        && reservation.routing_owner
    {
        store.abandon_router_mutation_routing_reservation(caller, logical_graph_name, key)?;
    }
    Ok(())
}

fn request_fingerprint(plan_blob: &[u8], params: &[u8], mode: GqlExecutionMode) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 8 + plan_blob.len() + 8 + params.len());
    out.push(match mode {
        GqlExecutionMode::Query => 0,
        GqlExecutionMode::Update => 1,
    });
    out.extend_from_slice(&(plan_blob.len() as u64).to_le_bytes());
    out.extend_from_slice(plan_blob);
    out.extend_from_slice(&(params.len() as u64).to_le_bytes());
    out.extend_from_slice(params);
    out
}

async fn apply_dispatch_label_telemetry_event(
    store: &RouterStore,
    graph_canister: Principal,
    shard_id: ShardId,
    expected_mutation_id: Option<MutationId>,
    event: &LabelTelemetryEventWire,
) -> Result<bool, RouterError> {
    if Some(event.mutation_id) != expected_mutation_id {
        return Err(RouterError::InvalidArgument(format!(
            "graph shard {shard_id} returned label telemetry event for mutation_id {}, expected {:?}",
            event.mutation_id, expected_mutation_id
        )));
    }
    let _ = store.apply_label_telemetry_event(shard_id, event);
    ack_label_telemetry_event(graph_canister, event.shard_event_seq)
        .await
        .map_err(RouterError::InvalidArgument)?;
    Ok(true)
}

async fn apply_and_ack_label_telemetry_events(
    store: &RouterStore,
    graph_canister: Principal,
    shard_id: ShardId,
    expected_mutation_id: Option<MutationId>,
    events: &[LabelTelemetryEventWire],
) -> Result<bool, RouterError> {
    for event in events {
        apply_dispatch_label_telemetry_event(
            store,
            graph_canister,
            shard_id,
            expected_mutation_id,
            event,
        )
        .await?;
    }
    Ok(true)
}

async fn reconcile_router_mutation_telemetry(
    store: &RouterStore,
    caller: Principal,
    logical_graph_name: &str,
    client_key: &str,
) -> Result<(), RouterError> {
    let Some(record) = store.router_mutation_record(caller, logical_graph_name, client_key) else {
        return Ok(());
    };
    for shard in record
        .shards
        .iter()
        .filter(|shard| shard.completed && !shard.telemetry_acked)
    {
        apply_and_ack_label_telemetry_events(
            store,
            shard.graph_canister,
            shard.shard_id,
            Some(record.mutation_id),
            &shard.label_telemetry_events,
        )
        .await?;
        store.record_router_mutation_shard_telemetry_acked(
            caller,
            logical_graph_name,
            client_key,
            shard.shard_id,
        )?;
    }
    Ok(())
}

async fn recover_mutation_outcome(
    store: &RouterStore,
    graph_canister: Principal,
    shard_id: ShardId,
    mutation_id: MutationId,
) -> Result<Option<gleaph_graph_kernel::plan_exec::MutationOutcomeWire>, RouterError> {
    if let Some(outcome) = get_mutation_outcome(graph_canister, mutation_id)
        .await
        .map_err(RouterError::InvalidArgument)?
    {
        for event in &outcome.label_telemetry_events {
            let _ = apply_dispatch_label_telemetry_event(
                store,
                graph_canister,
                shard_id,
                Some(mutation_id),
                event,
            )
            .await?;
        }
        return Ok(Some(outcome));
    }
    replay_pending_label_telemetry(store, graph_canister, shard_id, mutation_id).await?;
    Ok(None)
}

async fn replay_pending_label_telemetry(
    store: &RouterStore,
    graph_canister: Principal,
    shard_id: ShardId,
    mutation_id: MutationId,
) -> Result<(), RouterError> {
    const PENDING_REPLAY_LIMIT: u32 = 1_000;
    let mut from_seq = 0;
    loop {
        let events =
            list_pending_label_telemetry_events(graph_canister, from_seq, PENDING_REPLAY_LIMIT)
                .await
                .map_err(RouterError::InvalidArgument)?;
        if events.is_empty() {
            return Ok(());
        }
        for event in events
            .iter()
            .filter(|event| event.mutation_id == mutation_id)
        {
            let _ = store.apply_label_telemetry_event(shard_id, event);
            ack_label_telemetry_event(graph_canister, event.shard_event_seq)
                .await
                .map_err(RouterError::InvalidArgument)?;
        }
        let Some(next_seq) = events
            .last()
            .and_then(|event| event.shard_event_seq.checked_add(1))
        else {
            return Ok(());
        };
        if events.len() < PENDING_REPLAY_LIMIT as usize {
            return Ok(());
        }
        from_seq = next_seq;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;

    use candid::{Decode, Principal};
    use gleaph_gql::Value;
    use gleaph_gql::ast::CmpOp;
    use gleaph_gql_planner::plan::ScanValue;
    use gleaph_gql_planner::wire::encode_block_plans;
    use gleaph_gql_planner::{NodeLabelRef, PhysicalPlan, PlanOp};
    use gleaph_graph_kernel::index::PostingHit;
    use gleaph_graph_kernel::plan_exec::{
        GqlExecutionMode, LabelTelemetryEventWire, LabelUsageDelta, SeedBindingsWire,
    };

    use crate::facade::stable::label_telemetry::LabelStats;
    use crate::facade::store::RouterStore;
    use crate::federation::resolve_seed_routings_multi;
    use crate::gql::{
        IndexLookup, apply_dispatch_label_telemetry_event, dispatch_plan_blob_with_index,
        request_fingerprint,
    };
    use crate::init::RouterInitArgs;
    use crate::seed::{IndexAnchor, SeedProbe};
    use crate::state::RouterError;
    use crate::types::AdminRegisterShardArgs;

    fn graph_principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    fn store_with_shards() -> RouterStore {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        for (shard_id, graph_byte) in [(7u32, 1u8), (9, 4)] {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id,
                    graph_canister: graph_principal(graph_byte),
                    index_canister: graph_principal(2),
                    logical_graph_name: "tenant.main".into(),
                },
            ))
            .expect("register shard");
        }
        store
    }

    #[derive(Clone)]
    struct FakeIndex {
        calls: Rc<Cell<u32>>,
        results: Rc<RefCell<Vec<Result<Vec<PostingHit>, String>>>>,
    }

    impl FakeIndex {
        fn new(results: Vec<Result<Vec<PostingHit>, String>>) -> Self {
            Self {
                calls: Rc::new(Cell::new(0)),
                results: Rc::new(RefCell::new(results)),
            }
        }

        fn calls(&self) -> u32 {
            self.calls.get()
        }
    }

    impl IndexLookup for FakeIndex {
        fn lookup_equal(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            self.calls.set(self.calls.get() + 1);
            let result = self.results.borrow_mut().remove(0);
            Box::pin(async move { result })
        }

        fn lookup_intersection(
            &self,
            _req: gleaph_graph_kernel::index::IndexIntersectionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            self.calls.set(self.calls.get() + 1);
            let result = self.results.borrow_mut().remove(0);
            Box::pin(async move { result })
        }
    }

    fn seeded_dml_plan() -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::IndexScan {
                variable: Rc::from("u"),
                property: Rc::from("uid"),
                value: ScanValue::Literal(Value::Text("alice".into())),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::InsertVertex {
                variable: Some(Rc::from("n")),
                labels: vec![NodeLabelRef::from("Person")],
                properties: vec![],
            },
        ])
    }

    fn seeded_dml_bundle(plan: &PhysicalPlan) -> Vec<u8> {
        encode_block_plans(std::slice::from_ref(plan), true).expect("encode plan")
    }

    fn store_with_shards_and_property() -> RouterStore {
        let store = store_with_shards();
        let admin = Principal::anonymous();
        store
            .admin_intern_property(admin, "uid")
            .expect("intern uid");
        store
    }

    async fn dispatch_with_fake_index(
        store: &RouterStore,
        fake_index: &FakeIndex,
        plan: &PhysicalPlan,
        plan_blob: &[u8],
        client_key: &str,
    ) -> Result<u64, RouterError> {
        let shards = store
            .list_shards_for_graph("tenant.main")
            .expect("registered shards");
        dispatch_plan_blob_with_index(
            "tenant.main",
            plan_blob,
            std::slice::from_ref(plan),
            &BTreeMap::new(),
            &[],
            GqlExecutionMode::Update,
            Some(client_key),
            store,
            shards,
            fake_index,
            Principal::anonymous(),
        )
        .await
    }

    #[test]
    fn pre_dispatch_index_failure_releases_routing_owner_but_preserves_key_record() {
        let store = store_with_shards_and_property();
        let plan = seeded_dml_plan();
        let plan_blob = seeded_dml_bundle(&plan);
        let fake_index = FakeIndex::new(vec![Err("index unavailable".into())]);

        let err = futures::executor::block_on(dispatch_with_fake_index(
            &store,
            &fake_index,
            &plan,
            &plan_blob,
            "client-key-1",
        ))
        .expect_err("index failure");
        assert_eq!(
            err,
            RouterError::InvalidArgument("index unavailable".into())
        );
        assert_eq!(fake_index.calls(), 1);

        let record = store
            .router_mutation_record(Principal::anonymous(), "tenant.main", "client-key-1")
            .expect("mutation record");
        assert_eq!(record.mutation_id, 1);
        assert_eq!(
            record.request_fingerprint,
            request_fingerprint(&plan_blob, &[], GqlExecutionMode::Update)
        );
        assert!(!record.routing_in_progress);
        assert!(record.shards.is_empty());
        assert!(record.completed_row_count.is_none());

        let retry = store
            .reserve_mutation_id_for_client_key(
                Principal::anonymous(),
                "tenant.main",
                "client-key-1",
                request_fingerprint(&plan_blob, &[], GqlExecutionMode::Update),
            )
            .expect("retry reservation");
        assert_eq!(retry.mutation_id, record.mutation_id);
        assert!(retry.routing_owner);
        assert_eq!(
            store.reserve_mutation_id_for_client_key(
                Principal::anonymous(),
                "tenant.main",
                "client-key-1",
                b"different request".to_vec(),
            ),
            Err(RouterError::Conflict(
                "client_mutation_key was already used for a different request".into()
            ))
        );
    }

    #[test]
    fn zero_hit_seeded_dml_records_completed_zero_rows() {
        let store = store_with_shards_and_property();
        let plan = seeded_dml_plan();
        let plan_blob = seeded_dml_bundle(&plan);
        let fake_index = FakeIndex::new(vec![Ok(Vec::new())]);

        let rows = futures::executor::block_on(dispatch_with_fake_index(
            &store,
            &fake_index,
            &plan,
            &plan_blob,
            "client-key-1",
        ))
        .expect("zero-hit dispatch");
        assert_eq!(rows, 0);
        assert_eq!(fake_index.calls(), 1);

        let record = store
            .router_mutation_record(Principal::anonymous(), "tenant.main", "client-key-1")
            .expect("mutation record");
        assert_eq!(record.completed_row_count, Some(0));
        assert!(!record.routing_in_progress);
        assert!(record.shards.is_empty());

        let rows = futures::executor::block_on(dispatch_with_fake_index(
            &store,
            &fake_index,
            &plan,
            &plan_blob,
            "client-key-1",
        ))
        .expect("cached zero-hit retry");
        assert_eq!(rows, 0);
        assert_eq!(fake_index.calls(), 1);
    }

    #[test]
    fn successful_seeded_dml_records_envelope_before_shard_dispatch() {
        let store = store_with_shards_and_property();
        let plan = seeded_dml_plan();
        let plan_blob = seeded_dml_bundle(&plan);
        let fake_index = FakeIndex::new(vec![Ok(vec![PostingHit {
            shard_id: 7,
            vertex_id: 42,
        }])]);

        let err = futures::executor::block_on(dispatch_with_fake_index(
            &store,
            &fake_index,
            &plan,
            &plan_blob,
            "client-key-1",
        ))
        .expect_err("native graph dispatch should fail after envelope");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert_eq!(fake_index.calls(), 1);

        let record = store
            .router_mutation_record(Principal::anonymous(), "tenant.main", "client-key-1")
            .expect("mutation record");
        assert_eq!(record.mutation_id, 1);
        assert!(!record.routing_in_progress);
        assert!(record.completed_row_count.is_none());
        assert_eq!(record.shards.len(), 1);
        assert_eq!(record.shards[0].shard_id, 7);
        assert_eq!(record.shards[0].graph_canister, graph_principal(1));
        assert!(!record.shards[0].completed);

        let resolved = record.resolved_labels.expect("resolved labels");
        assert_eq!(resolved.vertex.len(), 1);
        assert_eq!(resolved.vertex[0].name, "Person");
        assert_eq!(resolved.vertex[0].id.raw(), 1);

        let seed_blob = record.shards[0]
            .seed_bindings_blob
            .as_ref()
            .expect("seed bindings");
        let seeds: SeedBindingsWire =
            candid::Decode!(seed_blob, SeedBindingsWire).expect("decode seeds");
        assert_eq!(seeds.entries.len(), 1);
        assert_eq!(seeds.entries[0].variable, "u");
        assert_eq!(seeds.entries[0].local_vertex_ids, vec![42]);
    }

    #[test]
    fn resolve_seed_routings_multi_fans_out_by_shard() {
        let store = store_with_shards();
        let probe = SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            payload_bytes: vec![1, 2, 3],
        };
        let hits = vec![
            PostingHit {
                shard_id: 7,
                vertex_id: 10,
            },
            PostingHit {
                shard_id: 9,
                vertex_id: 20,
            },
        ];
        let routings =
            resolve_seed_routings_multi(&store, &hits, "tenant.main", IndexAnchor::Equal(probe))
                .expect("route");
        assert_eq!(routings.len(), 2);
        assert_eq!(routings[0].shard_id, 7);
        assert_eq!(routings[1].shard_id, 9);
        assert_eq!(routings[0].hits.len(), 1);
        assert_eq!(routings[0].hits[0].vertex_id, 10);
        assert!(routings[0].anchor.is_some());
        assert_eq!(routings[0].graph_canister, graph_principal(1));
    }

    #[test]
    fn resolve_seed_routings_multi_rejects_unknown_shard() {
        let store = store_with_shards();
        let probe = SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            payload_bytes: vec![],
        };
        let hits = vec![PostingHit {
            shard_id: 99,
            vertex_id: 1,
        }];
        let err =
            resolve_seed_routings_multi(&store, &hits, "tenant.main", IndexAnchor::Equal(probe))
                .expect_err("unknown shard");
        assert!(matches!(err, RouterError::ShardNotRegistered));
    }

    #[test]
    fn mismatched_label_telemetry_mutation_id_is_rejected_before_apply() {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        let label = store
            .admin_intern_vertex_label(admin, "Person")
            .expect("label");
        let event = LabelTelemetryEventWire {
            mutation_id: 99,
            shard_event_seq: 1,
            label_usage_delta: LabelUsageDelta {
                vertex: vec![(label, 1)],
                edge: vec![],
            },
        };

        let err = futures::executor::block_on(apply_dispatch_label_telemetry_event(
            &store,
            graph_principal(1),
            7,
            Some(42),
            &event,
        ))
        .expect_err("mismatched event should fail");

        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert_eq!(store.vertex_label_stats(label), LabelStats::default());
    }
}
