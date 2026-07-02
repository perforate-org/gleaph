//! PocketIC coverage for the ADR 0031 Slice 3 Router vector-index catalog, target resolution, and
//! the fail-closed activation gate; plus ADR 0034 Slice 4/5 vector search through Router and GQL.
//!
//! This file consolidates ten former PocketIC bootstraps into five isolated lifecycle fixtures.
//! Former-test to scenario mapping:
//! - `register_resolve_and_backfill_stay_fail_closed` + `anonymous_target_is_rejected`
//!   -> `catalog_lifecycle_keeps_backfill_fail_closed_and_rejects_anonymous_target`
//! - `failed_registration_does_not_allocate_an_embedding_name`
//!   -> `dense_embedding_name_allocation_is_isolated_for_rejected_registrations`
//! - `activation_is_fenced_on_flag_and_shard_attach`
//!   + `vector_search_on_activated_empty_index_returns_empty`
//!   + `gql_search_empty_hits_runs_aggregate_and_returns_one_zero_row`
//!     -> `activation_flag_and_shard_attach_gate_empty_search_and_aggregate`
//! - `vector_search_returns_seeded_hit` + `gql_search_distance_as_executes_through_router_vector_index`
//!   -> `seeded_l2_search_orders_exact_subject_and_returns_element_id_distance`
//! - `gql_search_score_as_executes_through_router_vector_index_for_cosine`
//!   + `gql_search_distance_as_rejected_for_cosine_index`
//!     -> `cosine_score_as_is_exact_and_distance_as_rejected_without_poisoning`

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{RouterError, ShardId, VectorActivationBlockReason};
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorIndexError, VectorMetric, VectorSearchResult,
    VectorSubject,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, e2e_insert_vertex, gql_query_as_admin,
    gql_query_with_params_as_admin, install_federation, install_single_shard_federation,
    install_vector_canister,
};
use gleaph_router::types::{
    AdminAttachVectorIndexShardArgs, AdminVectorIndexBackfillStepArgs,
    AdminVectorIndexBackfillStepResult, RegisterVectorIndexArgs, RouterVectorSearchRequest,
    SetVectorIndexTargetArgs, VectorIndexActivationStateView, VectorIndexActivationStatus,
    VectorIndexInfo,
};
use std::collections::BTreeMap;

const EMBEDDING_NAME: &str = "adr0031_title_vec";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;

const TARGETLESS_INDEX_ID: u32 = 2;
const TARGETLESS_EMBEDDING_NAME: &str = "adr0031_targetless_vec";

fn register(env: &FederationEnv, args: &RegisterVectorIndexArgs) -> Result<bool, RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_register_vector_index",
            Encode!(args).expect("encode register args"),
        )
        .expect("admin_register_vector_index call");
    Decode!(&bytes, Result<bool, RouterError>).expect("decode register result")
}

fn activation_status(
    env: &FederationEnv,
    index_id: u32,
) -> Result<VectorIndexActivationStatus, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "vector_index_activation_status",
            Encode!(&GRAPH_NAME.to_string(), &index_id).expect("encode status args"),
        )
        .expect("vector_index_activation_status call");
    Decode!(&bytes, Result<VectorIndexActivationStatus, RouterError>).expect("decode status")
}

fn list(env: &FederationEnv) -> Result<Vec<VectorIndexInfo>, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_vector_indexes",
            Encode!(&GRAPH_NAME.to_string()).expect("encode list args"),
        )
        .expect("list_vector_indexes call");
    Decode!(&bytes, Result<Vec<VectorIndexInfo>, RouterError>).expect("decode list")
}

fn resolve_target(env: &FederationEnv, index_id: u32) -> Result<Principal, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "resolve_vector_index_target",
            Encode!(&GRAPH_NAME.to_string(), &index_id).expect("encode resolve args"),
        )
        .expect("resolve_vector_index_target call");
    Decode!(&bytes, Result<Principal, RouterError>).expect("decode resolve")
}

fn backfill_step(
    env: &FederationEnv,
    index_id: u32,
) -> Result<AdminVectorIndexBackfillStepResult, RouterError> {
    let args = AdminVectorIndexBackfillStepArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        index_id,
        shard_id: ShardId::new(0),
        start_vertex_id: 0,
        max_vertices: 256,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_vector_index_backfill_step",
            Encode!(&args).expect("encode backfill args"),
        )
        .expect("admin_vector_index_backfill_step call");
    Decode!(&bytes, Result<AdminVectorIndexBackfillStepResult, RouterError>)
        .expect("decode backfill result")
}

fn set_target(env: &FederationEnv, index_id: u32, target: Principal) -> Result<(), RouterError> {
    let args = SetVectorIndexTargetArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        index_id,
        target,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_vector_index_target",
            Encode!(&args).expect("encode set-target args"),
        )
        .expect("admin_set_vector_index_target call");
    Decode!(&bytes, Result<(), RouterError>).expect("decode set-target result")
}

#[test]
fn dense_embedding_name_allocation_is_isolated_for_rejected_registrations() {
    let env = install_federation();
    let target = env.index;

    let reg = |embedding_name: &str, index_id: u32, tgt: Option<Principal>, if_not_exists: bool| {
        register(
            &env,
            &RegisterVectorIndexArgs {
                logical_graph_name: GRAPH_NAME.to_string(),
                embedding_name: embedding_name.to_string(),
                index_id,
                dims: DIMS,
                metric: Some(VectorMetric::L2Squared),
                target: tgt,
                if_not_exists,
            },
        )
    };

    // Scenario: first successful registration interns "vec_a" -> dense id 1.
    assert!(
        reg("vec_a", 10, Some(target), false).expect("register vec_a"),
        "first registration is newly created"
    );

    // Every rejected or no-op registration must NOT advance the embedding-name counter:
    // 1) conflict on an existing index id,
    assert!(
        matches!(
            reg("leak_conflict", 10, Some(target), false),
            Err(RouterError::Conflict(_))
        ),
        "index-id conflict must fail without interning the name"
    );
    // 2) if-not-exists no-op on an existing index id,
    assert!(
        !reg("leak_ifne", 10, Some(target), true).expect("if-not-exists no-op"),
        "existing def with if_not_exists is a no-op"
    );
    // 3) anonymous target rejection on a fresh index id,
    assert!(
        matches!(
            reg("leak_anon", 11, Some(Principal::anonymous()), false),
            Err(RouterError::InvalidArgument(_))
        ),
        "anonymous target must be rejected before interning the name"
    );
    // 4) one-target-per-graph rejection on a fresh index id + fresh name.
    let other_target = Principal::from_slice(&[0x11; 29]);
    assert!(
        matches!(
            reg("leak_target", 13, Some(other_target), false),
            Err(RouterError::Conflict(_))
        ),
        "target conflict must fail closed before interning the name"
    );

    // Postcondition: the next successful registration receives dense id 2. Any leak would have
    // advanced the counter to 3+.
    assert!(
        reg("vec_next", 12, Some(target), false).expect("register vec_next"),
        "next successful registration is newly created"
    );
    let defs = list(&env).expect("list");
    let next = defs
        .iter()
        .find(|d| d.index_id == 12)
        .expect("vec_next def present");
    assert_eq!(
        next.embedding_name_id, 2,
        "failed registrations must not advance the dense embedding-name id"
    );
}

#[test]
fn catalog_lifecycle_keeps_backfill_fail_closed_and_rejects_anonymous_target() {
    let env = install_federation();
    let target = env.index;

    // Scenario: targeted registration / resolve / list / backfill-disabled.
    let created = register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            metric: Some(VectorMetric::L2Squared),
            target: Some(target),
            if_not_exists: false,
        },
    )
    .expect("register targeted vector index");
    assert!(created, "first registration is newly created");

    let status = activation_status(&env, INDEX_ID).expect("activation status");
    assert_eq!(
        status.activation_state,
        VectorIndexActivationStateView::DispatchBlocked,
        "targeted def stays DispatchBlocked until activation + attach"
    );
    assert!(
        status.blocked_reason.is_some(),
        "blocked state must carry an explanation"
    );

    assert_eq!(
        resolve_target(&env, INDEX_ID).expect("resolve target"),
        target,
        "resolve returns the registered target"
    );

    let defs = list(&env).expect("list");
    assert_eq!(defs.len(), 1, "exactly one registered index");
    assert_eq!(defs[0].index_id, INDEX_ID);
    assert_eq!(defs[0].dims, DIMS);
    assert_eq!(defs[0].target, Some(target));
    assert_ne!(
        defs[0].embedding_name_id, 0,
        "embedding name id 0 is reserved/unset"
    );

    assert!(
        matches!(
            backfill_step(&env, INDEX_ID),
            Err(RouterError::VectorDispatchActivationBlocked(
                VectorActivationBlockReason::DispatchNotActivated
            ))
        ),
        "backfill must fail closed while the global activation flag is off"
    );

    // Scenario: targetless definition rejects an anonymous target and stays targetless.
    assert!(
        register(
            &env,
            &RegisterVectorIndexArgs {
                logical_graph_name: GRAPH_NAME.to_string(),
                embedding_name: TARGETLESS_EMBEDDING_NAME.to_string(),
                index_id: TARGETLESS_INDEX_ID,
                dims: DIMS,
                metric: Some(VectorMetric::L2Squared),
                target: None,
                if_not_exists: false,
            },
        )
        .expect("register targetless vector index"),
        "targetless registration is newly created"
    );

    assert!(
        matches!(
            set_target(&env, TARGETLESS_INDEX_ID, Principal::anonymous()),
            Err(RouterError::InvalidArgument(_))
        ),
        "anonymous target principal must be rejected"
    );

    // Adversarial postcondition: the error must not have mutated the definition.
    let status = activation_status(&env, TARGETLESS_INDEX_ID).expect("activation status");
    assert_eq!(
        status.activation_state,
        VectorIndexActivationStateView::Registered,
        "targetless def must stay Registered after anonymous-target rejection"
    );
    assert!(
        status.blocked_reason.is_none(),
        "Registered state has no blocked reason"
    );

    let defs = list(&env).expect("list after rejection");
    let targetless = defs
        .iter()
        .find(|d| d.index_id == TARGETLESS_INDEX_ID)
        .expect("targetless def present");
    assert!(
        targetless.target.is_none(),
        "targetless def must remain targetless after the rejected set_target call"
    );
}

fn set_dispatch_activation(env: &FederationEnv, enabled: bool) -> Result<(), RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_vector_dispatch_activation",
            Encode!(&enabled).expect("encode activation flag"),
        )
        .expect("admin_set_vector_dispatch_activation call");
    Decode!(&bytes, Result<(), RouterError>).expect("decode activation result")
}

fn dispatch_activation_enabled(env: &FederationEnv) -> bool {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "vector_dispatch_activation_enabled",
            Encode!().expect("encode activation query"),
        )
        .expect("vector_dispatch_activation_enabled call");
    Decode!(&bytes, bool).expect("decode activation flag")
}

fn attach_shard(
    env: &FederationEnv,
    shard_id: ShardId,
    vector_index_canister: Principal,
) -> Result<(), RouterError> {
    let args = AdminAttachVectorIndexShardArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        shard_id,
        vector_index_canister,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_attach_vector_index_shard",
            Encode!(&args).expect("encode attach args"),
        )
        .expect("admin_attach_vector_index_shard call");
    Decode!(&bytes, Result<(), RouterError>).expect("decode attach result")
}

fn router_graph_id(env: &FederationEnv) -> GraphId {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "lookup_graph_id",
            Encode!(&GRAPH_NAME.to_string()).expect("encode lookup_graph_id"),
        )
        .expect("lookup_graph_id call");
    Decode!(&bytes, Result<GraphId, RouterError>)
        .expect("decode lookup_graph_id")
        .expect("graph id")
}

fn attach_shard_to_vector(
    env: &FederationEnv,
    vector: Principal,
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister: Principal,
) -> Result<(), String> {
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &shard_id, &shard_canister).expect("encode vector attach"),
        )
        .expect("vector admin_attach_shard_canister call");
    Decode!(&bytes, Result<(), String>).expect("decode vector attach")
}

fn set_graph_vector_routing(env: &FederationEnv, graph: Principal, vector: Principal) {
    let bytes = env
        .pic
        .update_call(
            graph,
            env.router,
            "admin_set_vector_index_canister",
            Encode!(&vector).expect("encode set vector routing"),
        )
        .expect("admin_set_vector_index_canister call");
    Decode!(&bytes, Result<(), String>)
        .expect("decode set vector routing")
        .expect("graph accepts router-set vector routing");
}

/// Activate the vector index for a single-shard graph (used by GQL search fixtures).
fn fully_activate_single_shard_index(env: &FederationEnv, vector: Principal) {
    set_dispatch_activation(env, true).expect("enable dispatch flag");
    let graph_id = router_graph_id(env);
    set_graph_vector_routing(env, env.graph_source, vector);
    attach_shard_to_vector(env, vector, graph_id, ShardId::new(0), env.graph_source)
        .expect("vector accepts shard 0");
    attach_shard(env, ShardId::new(0), vector).expect("attach shard 0");
}

#[test]
fn activation_flag_and_shard_attach_gate_empty_search_and_aggregate() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            metric: Some(VectorMetric::L2Squared),
            target: Some(vector),
            if_not_exists: false,
        },
    )
    .expect("register vector index");

    // Gate 1 only: flag on, no shard attach -> blocked on shard attachment.
    set_dispatch_activation(&env, true).expect("enable dispatch flag");
    assert!(dispatch_activation_enabled(&env), "flag is now on");
    assert!(
        matches!(
            backfill_step(&env, INDEX_ID),
            Err(RouterError::VectorDispatchActivationBlocked(
                VectorActivationBlockReason::ShardsNotVectorAttached
            ))
        ),
        "flag alone is insufficient: shards are not vector-attached yet"
    );

    // Wire graph/vector handshakes explicitly so we can observe the partial-shard gate.
    let graph_id = router_graph_id(&env);
    set_graph_vector_routing(&env, env.graph_source, vector);
    set_graph_vector_routing(&env, env.graph_dest, vector);
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(0), env.graph_source)
        .expect("vector accepts shard 0");
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(1), env.graph_dest)
        .expect("single vector target accepts shard 1");

    // Attach only shard 0 via the Router catalog: activation and backfill must stay blocked.
    attach_shard(&env, ShardId::new(0), vector).expect("attach shard 0");
    assert!(
        matches!(
            activation_status(&env, INDEX_ID)
                .expect("activation status")
                .activation_state,
            VectorIndexActivationStateView::DispatchBlocked
        ),
        "shard 1 still missing -> DispatchBlocked"
    );
    assert!(
        matches!(
            backfill_step(&env, INDEX_ID),
            Err(RouterError::VectorDispatchActivationBlocked(
                VectorActivationBlockReason::ShardsNotVectorAttached
            ))
        ),
        "shard 0 alone is insufficient: shard 1 is not vector-attached yet"
    );

    // Attach shard 1 -> both gates pass.
    attach_shard(&env, ShardId::new(1), vector).expect("attach shard 1");
    let status = activation_status(&env, INDEX_ID).expect("activation status");
    assert_eq!(
        status.activation_state,
        VectorIndexActivationStateView::DispatchEnabled,
        "flag on + all shards attached -> DispatchEnabled"
    );
    assert!(
        status.blocked_reason.is_none(),
        "enabled state has no reason"
    );

    // Bounded empty backfill converges immediately.
    let step = backfill_step(&env, INDEX_ID).expect("backfill step runs once dispatch is ready");
    assert_eq!(step.shard_id, ShardId::new(0));
    assert!(step.done, "empty shard converges in a single step");
    assert_eq!(step.embeddings_synced, 0);

    // Attach is idempotent.
    attach_shard(&env, ShardId::new(0), vector).expect("re-attach is idempotent");
    assert_eq!(
        activation_status(&env, INDEX_ID)
            .expect("status")
            .activation_state,
        VectorIndexActivationStateView::DispatchEnabled,
        "idempotent attach keeps DispatchEnabled"
    );

    // Scenario: activated-empty Router search returns no hits.
    let router_result = router_vector_search(&env, 1.0, 10).expect("router search on empty index");
    assert!(
        router_result.hits.is_empty(),
        "activated empty index must return zero router hits"
    );

    // Scenario: empty leading-search aggregate dispatches the stripped tail and returns one zero row.
    let query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance RETURN count(*) AS c"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode query params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 1,
        "global aggregate over empty leading search must return one row"
    );
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let columns: BTreeMap<String, gleaph_gql_ic::IcWireValue> =
        wire.rows[0].columns.clone().into_iter().collect();
    match columns.get("c").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(cnt) => {
            assert_eq!(*cnt, 0, "count over empty search hits must be 0");
        }
        other => panic!("count must be Int64, got {other:?}"),
    }

    // Re-fence dispatch by disabling the global flag.
    set_dispatch_activation(&env, false).expect("disable dispatch flag");
    assert!(!dispatch_activation_enabled(&env), "flag is now off");
    assert!(
        matches!(
            backfill_step(&env, INDEX_ID),
            Err(RouterError::VectorDispatchActivationBlocked(
                VectorActivationBlockReason::DispatchNotActivated
            ))
        ),
        "global flag is the outermost fence"
    );
}

fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DIMS as usize * 4);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn seed_embedding(
    env: &FederationEnv,
    vector: Principal,
    shard_canister: Principal,
    vertex_id: u32,
    value: f32,
) -> Result<(), VectorIndexError> {
    let op = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id,
        },
        embedding_incarnation: 1,
        embedding_version: 1,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        bytes: vec_bytes(value),
        remove: false,
    };
    let bytes = env
        .pic
        .update_call(
            vector,
            shard_canister,
            "vector_upsert",
            Encode!(&op).expect("encode upsert op"),
        )
        .expect("vector_upsert call");
    Decode!(&bytes, Result<(), VectorIndexError>).expect("decode upsert result")
}

fn seed_embedding_with_metric(
    env: &FederationEnv,
    vector: Principal,
    shard_canister: Principal,
    vertex_id: u32,
    value: f32,
    metric: VectorMetric,
) -> Result<(), VectorIndexError> {
    let op = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id,
        },
        embedding_incarnation: 1,
        embedding_version: 1,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric,
        bytes: vec_bytes(value),
        remove: false,
    };
    let bytes = env
        .pic
        .update_call(
            vector,
            shard_canister,
            "vector_upsert",
            Encode!(&op).expect("encode upsert op"),
        )
        .expect("vector_upsert call");
    Decode!(&bytes, Result<(), VectorIndexError>).expect("decode upsert result")
}

fn router_vector_search(
    env: &FederationEnv,
    query_value: f32,
    top_k: u32,
) -> Result<VectorSearchResult, RouterError> {
    let req = RouterVectorSearchRequest {
        logical_graph_name: GRAPH_NAME.to_string(),
        index_id: INDEX_ID,
        query: vec_bytes(query_value),
        dims: DIMS,
        top_k,
    };
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "vector_search",
            Encode!(&req).expect("encode router vector search"),
        )
        .expect("router vector_search call");
    Decode!(&bytes, Result<VectorSearchResult, RouterError>).expect("decode router search result")
}

fn vertex_element_id(env: &FederationEnv) -> gleaph_gql_ic::IcWireValue {
    let result = gql_query_as_admin(env, "MATCH (v) RETURN ELEMENT_ID(v) AS v_id");
    assert_eq!(
        result.row_count, 1,
        "expected exactly one graph vertex for element-id lookup"
    );
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let mut columns: BTreeMap<String, gleaph_gql_ic::IcWireValue> = wire
        .rows
        .into_iter()
        .next()
        .expect("one row")
        .columns
        .into_iter()
        .collect();
    columns
        .remove("v_id")
        .expect("ELEMENT_ID(v) column present")
}

fn extract_id_and_distance(
    row: &gleaph_gql_ic::IcWirePlanQueryRow,
) -> (gleaph_gql_ic::IcWireValue, f64) {
    let columns: BTreeMap<String, gleaph_gql_ic::IcWireValue> = row
        .columns
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let id = columns.get("d_id").expect("d_id column present").clone();
    let distance = match columns.get("distance").expect("distance column present") {
        gleaph_gql_ic::IcWireValue::Float64(d) => *d,
        other => panic!("distance must be Float64, got {other:?}"),
    };
    (id, distance)
}

fn extract_id_and_score(
    row: &gleaph_gql_ic::IcWirePlanQueryRow,
) -> (gleaph_gql_ic::IcWireValue, f64) {
    let columns: BTreeMap<String, gleaph_gql_ic::IcWireValue> = row
        .columns
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let id = columns.get("d_id").expect("d_id column present").clone();
    let score = match columns.get("score").expect("score column present") {
        gleaph_gql_ic::IcWireValue::Float64(s) => *s,
        other => panic!("score must be Float64, got {other:?}"),
    };
    (id, score)
}

#[test]
fn seeded_l2_search_orders_exact_subject_and_returns_element_id_distance() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            metric: Some(VectorMetric::L2Squared),
            target: Some(vector),
            if_not_exists: false,
        },
    )
    .expect("register vector index");

    fully_activate_single_shard_index(&env, vector);

    // Seed two adversarial subjects directly on the vector canister.
    seed_embedding(&env, vector, env.graph_source, 101, 1.0).expect("seed raw subject 101");
    seed_embedding(&env, vector, env.graph_source, 102, 5.0).expect("seed raw subject 102");

    // Insert a graph vertex and seed an embedding that is an exact match for the query.
    let inserted = e2e_insert_vertex(&env, env.graph_source);
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        inserted.local_vertex_id,
        6.0,
    )
    .expect("seed embedding for inserted vertex");

    // Scenario: Router search orders by distance and reports the exact nearest subject/distance.
    let router_result = router_vector_search(&env, 6.0, 10).expect("router search");
    assert_eq!(
        router_result.hits.len(),
        3,
        "all three seeded vectors are router candidates"
    );
    let nearest = &router_result.hits[0];
    assert_eq!(
        nearest.subject,
        VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: inserted.local_vertex_id,
        },
        "nearest subject must be the inserted graph vertex"
    );
    assert_eq!(nearest.distance, 0.0, "exact match has zero distance");
    assert_eq!(nearest.embedding_incarnation, 1);
    assert_eq!(nearest.embedding_version, 1);
    assert!(
        router_result.hits[1].distance > 0.0,
        "the second hit must be farther than the exact match"
    );

    // Scenario: GQL DISTANCE AS returns the exact graph ELEMENT_ID with distance zero.
    let expected_id = vertex_element_id(&env);
    let query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance RETURN ELEMENT_ID(d) AS d_id, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(6.0)),
    )])
    .expect("encode query params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 1,
        "exact L2 query should return the seeded graph vertex, not a row count"
    );
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let (id, distance) = extract_id_and_distance(&wire.rows[0]);
    assert_eq!(
        id, expected_id,
        "GQL DISTANCE AS must return the exact inserted vertex ELEMENT_ID"
    );
    assert!(
        (distance - 0.0f64).abs() < 1e-6,
        "exact match distance must be zero"
    );
}

#[test]
fn cosine_score_as_is_exact_and_distance_as_rejected_without_poisoning() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            metric: Some(VectorMetric::Cosine),
            target: Some(vector),
            if_not_exists: false,
        },
    )
    .expect("register cosine vector index");

    fully_activate_single_shard_index(&env, vector);

    let inserted = e2e_insert_vertex(&env, env.graph_source);
    seed_embedding_with_metric(
        &env,
        vector,
        env.graph_source,
        inserted.local_vertex_id,
        5.0,
        VectorMetric::Cosine,
    )
    .expect("seed cosine embedding");

    let expected_id = vertex_element_id(&env);
    let score_query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) SCORE AS score RETURN ELEMENT_ID(d) AS d_id, score"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode query params");

    let first_score = {
        let result = gql_query_with_params_as_admin(&env, &score_query, params.clone());
        assert_eq!(
            result.row_count, 1,
            "exact cosine query should return the seeded vertex"
        );
        let rows_blob = result.rows_blob.expect("rows blob");
        let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
        assert_eq!(wire.rows.len(), 1);
        let (id, score) = extract_id_and_score(&wire.rows[0]);
        assert_eq!(
            id, expected_id,
            "GQL SCORE AS must return the exact inserted vertex ELEMENT_ID"
        );
        assert!(
            (score - 1.0f64).abs() < 1e-6,
            "identical directions must score ~1.0"
        );
        (id, score)
    };

    // Scenario: DISTANCE AS is rejected for a cosine index.
    let distance_query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance RETURN d, distance"
    );
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query",
            Encode!(&distance_query.to_string(), &params).expect("encode gql_query"),
        )
        .expect("gql_query call");
    let result: Result<GqlQueryResult, RouterError> =
        Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query result");
    let err = result.expect_err("DISTANCE AS on cosine must fail");
    assert!(
        err.to_string().contains("not supported for metric"),
        "unexpected error: {err}"
    );

    // Adversarial postcondition: the rejected DISTANCE AS must not poison later SCORE AS.
    let second_score = {
        let result = gql_query_with_params_as_admin(&env, &score_query, params.clone());
        assert_eq!(result.row_count, 1, "SCORE AS still returns one row");
        let rows_blob = result.rows_blob.expect("rows blob");
        let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
        assert_eq!(wire.rows.len(), 1);
        extract_id_and_score(&wire.rows[0])
    };
    assert_eq!(
        first_score, second_score,
        "SCORE AS result must be unchanged after the rejected DISTANCE AS query"
    );
}
