//! PocketIC coverage for the ADR 0031 Slice 3 Router vector-index catalog, target resolution, and
//! the fail-closed activation gate.
//!
//! Slice 3 makes vector dispatch addressable from the Router (register by embedding **name**, set a
//! single target, list, inspect activation status / resolve target). Production dispatch/backfill
//! stay **fail-closed**: with the global activation flag off (the default) a targeted definition
//! sits at `DispatchBlocked` and the backfill admin surface returns
//! `VectorDispatchActivationBlocked { DispatchNotActivated }` (ADR 0031 Slice 4).

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{RouterError, ShardId, VectorActivationBlockReason};
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorIndexError, VectorSearchResult, VectorSubject,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, e2e_insert_vertex, gql_query_with_params_as_admin,
    install_federation, install_vector_canister,
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
fn register_resolve_and_backfill_stay_fail_closed() {
    let env = install_federation();
    let target = env.index; // any non-anonymous principal

    // Register a targeted definition by embedding name (never a raw id).
    let created = register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            target: Some(target),
            if_not_exists: false,
        },
    )
    .expect("register vector index");
    assert!(created, "first registration is newly created");

    // A targeted definition is blocked while the global activation flag is off, with a reason.
    let status = activation_status(&env, INDEX_ID).expect("activation status");
    assert_eq!(
        status.activation_state,
        VectorIndexActivationStateView::DispatchBlocked,
        "fail-closed: a targeted def stays DispatchBlocked until activation + attach"
    );
    assert!(
        status.blocked_reason.is_some(),
        "blocked state must carry an explanation"
    );

    // Single-target resolution returns the catalog-local canister (inspect-only).
    assert_eq!(
        resolve_target(&env, INDEX_ID).expect("resolve target"),
        target
    );

    // The definition is listed for the graph with the Router-interned embedding-name id.
    let defs = list(&env);
    let defs = defs.expect("list");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].index_id, INDEX_ID);
    assert_eq!(defs[0].dims, DIMS);
    assert_eq!(defs[0].target, Some(target));
    assert_ne!(
        defs[0].embedding_name_id, 0,
        "embedding name id 0 is reserved/unset"
    );

    // The backfill admin surface fails closed for production while activation is off.
    assert!(
        matches!(
            backfill_step(&env, INDEX_ID),
            Err(RouterError::VectorDispatchActivationBlocked(
                VectorActivationBlockReason::DispatchNotActivated
            ))
        ),
        "backfill must fail closed while the global activation flag is off"
    );
}

#[test]
fn failed_registration_does_not_allocate_an_embedding_name() {
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
                target: tgt,
                if_not_exists,
            },
        )
    };

    // First successful registration interns "vec_a" -> dense id 1.
    assert!(reg("vec_a", 10, Some(target), false).expect("register vec_a"));

    // Three failure modes that MUST NOT intern their (otherwise-unused) embedding names:
    // 1) conflict on an existing index id,
    assert!(matches!(
        reg("leak_conflict", 10, Some(target), false),
        Err(RouterError::Conflict(_))
    ));
    // 2) if-not-exists no-op on an existing index id,
    assert!(
        !reg("leak_ifne", 10, Some(target), true).expect("if-not-exists no-op"),
        "existing def with if_not_exists is a no-op"
    );
    // 3) anonymous target rejection on a fresh index id.
    assert!(matches!(
        reg("leak_anon", 11, Some(Principal::anonymous()), false),
        Err(RouterError::InvalidArgument(_))
    ));
    // 4) one-target-per-graph rejection on a fresh index id + fresh name: the conflict must fail
    //    closed *before* interning, so it cannot leak an EmbeddingNameId either.
    let other_target = Principal::from_slice(&[0x11; 29]);
    assert!(matches!(
        reg("leak_target", 13, Some(other_target), false),
        Err(RouterError::Conflict(_))
    ));

    // The next successful registration must receive dense id 2 — proving none of the failed
    // registrations leaked an EmbeddingNameId. (A leak would have advanced the counter to 3+.)
    assert!(reg("vec_next", 12, Some(target), false).expect("register vec_next"));
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

/// Directly drive the vector canister's `admin_attach_shard_canister` (sender = router) the way the
/// production router would. Under `pocket-ic-e2e` the router records the registry bit but skips the
/// real inter-canister attach, so the test exercises it here to prove a single vector target accepts
/// every shard of the graph (ADR 0031 Slice 4 target model B).
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

/// Mirror the e2e harness contract for the index attach handshake: under `pocket-ic-e2e` the router
/// records the registry bit but skips the inter-canister calls, so the test drives the graph-local
/// routing update itself (sender = router, satisfying `guard_router_canister`).
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

/// ADR 0031 Slice 4/5: dispatch stays fenced until BOTH the global activation flag is on AND every
/// live shard has completed the vector-attach handshake. Once both gates pass, the definition flips
/// to `DispatchEnabled` and the bounded backfill driver runs instead of failing closed.
#[test]
fn activation_is_fenced_on_flag_and_shard_attach() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            target: Some(vector),
            if_not_exists: false,
        },
    )
    .expect("register vector index");

    // Gate 1 only (flag on, no shards attached) -> still blocked on shard attachment.
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

    // Complete the attach handshake for every live shard of the graph. Under the e2e harness the
    // router records readiness but skips the inter-canister wiring, so drive both legs ourselves
    // (the production router does this in `finish_shard_vector_attach`): the graph-local routing
    // update AND the real attach to the single vector target. The graph is two shards, so attaching
    // shard 1 to the SAME vector canister as shard 0 must succeed — the regression guard for the
    // property-index group ownership model that previously split a multi-shard graph across groups.
    let graph_id = router_graph_id(&env);
    set_graph_vector_routing(&env, env.graph_source, vector);
    set_graph_vector_routing(&env, env.graph_dest, vector);
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(0), env.graph_source)
        .expect("vector accepts shard 0");
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(1), env.graph_dest)
        .expect("single vector target accepts shard 1 of the same graph");
    attach_shard(&env, ShardId::new(0), vector).expect("attach shard 0");
    attach_shard(&env, ShardId::new(1), vector).expect("attach shard 1");

    // Both gates pass -> the definition reports DispatchEnabled.
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

    // The bounded backfill driver now runs (no embeddings seeded -> a clean, done step).
    let step = backfill_step(&env, INDEX_ID).expect("backfill step runs once dispatch is ready");
    assert_eq!(step.shard_id, ShardId::new(0));
    assert!(step.done, "empty shard converges in a single step");
    assert_eq!(step.embeddings_synced, 0);

    // Attach is idempotent: replaying it keeps the graph dispatch-ready.
    attach_shard(&env, ShardId::new(0), vector).expect("re-attach is idempotent");
    assert_eq!(
        activation_status(&env, INDEX_ID)
            .expect("status")
            .activation_state,
        VectorIndexActivationStateView::DispatchEnabled,
    );

    // Flipping the global flag back off re-fences dispatch even with shards attached.
    set_dispatch_activation(&env, false).expect("disable dispatch flag");
    assert!(!dispatch_activation_enabled(&env));
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

/// `DIMS` little-endian `f32` components each equal to `value` (matches the unit-test convention).
fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DIMS as usize * 4);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Seed one derived embedding by calling the vector canister directly as a shard owner (sender =
/// `shard_canister`), mirroring what the production sync path delivers. Under `pocket-ic-e2e` the
/// router does not auto-deliver mutations, so the test plants the row itself.
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

/// ADR 0031 Slice 5: once the Slice 4 activation gate is satisfied, the Router composite query routes
/// an exact search to the vector canister and returns the seeded nearest neighbor.
#[test]
fn vector_search_returns_seeded_hit() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            target: Some(vector),
            if_not_exists: false,
        },
    )
    .expect("register vector index");

    // Drive the full Slice 4 readiness handshake (flag + both shards attached to the single target).
    let graph_id = router_graph_id(&env);
    set_dispatch_activation(&env, true).expect("enable dispatch flag");
    set_graph_vector_routing(&env, env.graph_source, vector);
    set_graph_vector_routing(&env, env.graph_dest, vector);
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(0), env.graph_source)
        .expect("vector accepts shard 0");
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(1), env.graph_dest)
        .expect("vector accepts shard 1");
    attach_shard(&env, ShardId::new(0), vector).expect("attach shard 0");
    attach_shard(&env, ShardId::new(1), vector).expect("attach shard 1");

    // Seed two vectors on shard 0; the query equals the second exactly (distance 0).
    seed_embedding(&env, vector, env.graph_source, 1, 1.0).expect("seed v1");
    seed_embedding(&env, vector, env.graph_source, 2, 5.0).expect("seed v2");

    let result = router_vector_search(&env, 5.0, 10).expect("router search");
    assert_eq!(result.hits.len(), 2, "both seeded vectors are candidates");
    let nearest = &result.hits[0];
    assert_eq!(
        nearest.subject,
        VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: 2,
        }
    );
    assert_eq!(nearest.distance, 0.0);
    assert_eq!(nearest.embedding_incarnation, 1);
    assert_eq!(nearest.embedding_version, 1);
    assert!(
        result.hits[1].distance > 0.0,
        "the farther vector ranks second"
    );
}

/// ADR 0031 Slice 5 (P2): an activated index with no embeddings yet must return an empty result
/// through the full Router path, not fail at the vector canister (the physical def is created lazily
/// on first upsert, so an activated-but-empty index has no def).
#[test]
fn vector_search_on_activated_empty_index_returns_empty() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            target: Some(vector),
            if_not_exists: false,
        },
    )
    .expect("register vector index");

    let graph_id = router_graph_id(&env);
    set_dispatch_activation(&env, true).expect("enable dispatch flag");
    set_graph_vector_routing(&env, env.graph_source, vector);
    set_graph_vector_routing(&env, env.graph_dest, vector);
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(0), env.graph_source)
        .expect("vector accepts shard 0");
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(1), env.graph_dest)
        .expect("vector accepts shard 1");
    attach_shard(&env, ShardId::new(0), vector).expect("attach shard 0");
    attach_shard(&env, ShardId::new(1), vector).expect("attach shard 1");

    // No embeddings seeded -> the physical def does not exist yet, but the search must still succeed.
    let result = router_vector_search(&env, 1.0, 10).expect("router search on empty index");
    assert!(result.hits.is_empty());
}

#[test]
fn gql_search_distance_as_executes_through_router_vector_index() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            target: Some(vector),
            if_not_exists: false,
        },
    )
    .expect("register vector index");

    let graph_id = router_graph_id(&env);
    set_dispatch_activation(&env, true).expect("enable dispatch flag");
    set_graph_vector_routing(&env, env.graph_source, vector);
    set_graph_vector_routing(&env, env.graph_dest, vector);
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(0), env.graph_source)
        .expect("vector accepts shard 0");
    attach_shard_to_vector(&env, vector, graph_id, ShardId::new(1), env.graph_dest)
        .expect("vector accepts shard 1");
    attach_shard(&env, ShardId::new(0), vector).expect("attach shard 0");
    attach_shard(&env, ShardId::new(1), vector).expect("attach shard 1");

    // Insert a vertex on shard 0 and seed an embedding for it with value 5.0.
    let inserted = e2e_insert_vertex(&env, env.graph_source);
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        inserted.local_vertex_id,
        5.0,
    )
    .expect("seed embedding for inserted vertex");

    let query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance RETURN d, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode query params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 1,
        "exact query should return the seeded vertex"
    );
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let row = wire.rows.into_iter().next().expect("one row");
    let columns: BTreeMap<String, gleaph_gql_ic::IcWireValue> = row.columns.into_iter().collect();
    assert!(columns.contains_key("d"));
    assert!(columns.contains_key("distance"));
}

#[test]
fn anonymous_target_is_rejected() {
    let env = install_federation();
    // Register without a target -> Registered, then attempt an anonymous target.
    register(
        &env,
        &RegisterVectorIndexArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            embedding_name: EMBEDDING_NAME.to_string(),
            index_id: INDEX_ID,
            dims: DIMS,
            target: None,
            if_not_exists: false,
        },
    )
    .expect("register vector index");

    let status = activation_status(&env, INDEX_ID).expect("activation status");
    assert_eq!(
        status.activation_state,
        VectorIndexActivationStateView::Registered,
        "no target yet -> Registered"
    );
    assert!(status.blocked_reason.is_none());

    assert!(
        matches!(
            set_target(&env, INDEX_ID, Principal::anonymous()),
            Err(RouterError::InvalidArgument(_))
        ),
        "anonymous target principal must be rejected"
    );
}
