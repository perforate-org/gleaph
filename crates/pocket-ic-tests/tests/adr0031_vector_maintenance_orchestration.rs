//! PocketIC coverage for ADR 0031 Slice 10: Router-forwarded vector maintenance orchestration.
//!
//! The Router owns the maintenance policy (SSOT, disabled by default) and forwards bounded steps to
//! the vector canister, which owns the maintenance execution state. One Router push call advances at
//! most one bounded unit; the run stops at `ReadyToPublish` and publish stays an explicit forwarded
//! operation. These tests drive the *Router* surface (not the vector canister directly) so the full
//! resolve + RBAC + readiness + forward path is exercised end to end.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorIndexError, VectorMaintenancePolicy,
    VectorMaintenanceRecommendation, VectorMaintenanceStepResult, VectorRebuildPhase,
    VectorSearchResult, VectorSubject,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, install_federation, install_vector_canister,
};
use gleaph_router::types::{
    RegisterVectorIndexArgs, SetVectorMaintenancePolicyArgs, VectorMaintenanceStateView,
    VectorMaintenanceStatusView, VectorMaintenanceStepOutcome,
};

const EMBEDDING_NAME: &str = "adr0031_maint_vec";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 4;
const MAX_STEPS: usize = 64;

fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DIMS as usize * 4);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
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

fn register(env: &FederationEnv, target: Principal) {
    let args = RegisterVectorIndexArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: EMBEDDING_NAME.to_string(),
        index_id: INDEX_ID,
        dims: DIMS,
        target: Some(target),
        if_not_exists: false,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_register_vector_index",
            Encode!(&args).expect("encode register args"),
        )
        .expect("admin_register_vector_index call");
    Decode!(&bytes, Result<bool, RouterError>)
        .expect("decode register")
        .expect("register ok");
}

fn set_dispatch_activation(env: &FederationEnv, enabled: bool) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_vector_dispatch_activation",
            Encode!(&enabled).expect("encode activation flag"),
        )
        .expect("admin_set_vector_dispatch_activation call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode activation")
        .expect("activation ok");
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

fn attach_shard_to_vector(
    env: &FederationEnv,
    vector: Principal,
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister: Principal,
) {
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &shard_id, &shard_canister).expect("encode vector attach"),
        )
        .expect("vector admin_attach_shard_canister call");
    Decode!(&bytes, Result<(), String>)
        .expect("decode vector attach")
        .expect("vector accepts shard");
}

fn attach_shard(env: &FederationEnv, shard_id: ShardId, vector: Principal) {
    use gleaph_router::types::AdminAttachVectorIndexShardArgs;
    let args = AdminAttachVectorIndexShardArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        shard_id,
        vector_index_canister: vector,
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
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode attach")
        .expect("attach ok");
}

fn seed_embedding(
    env: &FederationEnv,
    vector: Principal,
    vertex_id: u32,
    version: u64,
    value: f32,
) {
    let op = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id,
        },
        embedding_incarnation: 1,
        embedding_version: version,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        bytes: vec_bytes(value),
        remove: false,
    };
    let bytes = env
        .pic
        .update_call(
            vector,
            env.graph_source,
            "vector_upsert",
            Encode!(&op).expect("encode upsert op"),
        )
        .expect("vector_upsert call");
    Decode!(&bytes, Result<(), VectorIndexError>)
        .expect("decode upsert")
        .expect("upsert ok");
}

/// Full Slice 4 activation handshake (flag + per-shard routing/attach to the single target) so the
/// Router maintenance surface resolves a ready target, then seed 5 rows / 1 tombstone at version 1.
fn ready_activated_vector_with_tombstone(env: &FederationEnv) -> Principal {
    let vector = install_vector_canister(&env.pic, env.router);
    register(env, vector);
    let graph_id = router_graph_id(env);
    set_dispatch_activation(env, true);
    set_graph_vector_routing(env, env.graph_source, vector);
    set_graph_vector_routing(env, env.graph_dest, vector);
    attach_shard_to_vector(env, vector, graph_id, ShardId::new(0), env.graph_source);
    attach_shard_to_vector(env, vector, graph_id, ShardId::new(1), env.graph_dest);
    attach_shard(env, ShardId::new(0), vector);
    attach_shard(env, ShardId::new(1), vector);
    for v in 1..=4u32 {
        seed_embedding(env, vector, v, 1, (v - 1) as f32);
    }
    seed_embedding(env, vector, 1, 2, 9.0); // tombstones subject 1's v1 row: 5 rows, 4 live, 1 dead
    vector
}

/// Tombstone-dominant policy (`tombstoned/total >= 20%` required); skew disabled by an unreachable
/// threshold so the degenerate `nlist = 1` fixture is judged on tombstones alone.
fn tombstone_required_policy() -> VectorMaintenancePolicy {
    VectorMaintenancePolicy {
        recommended_tombstone_ratio_bps: 1_000,
        required_tombstone_ratio_bps: 2_000,
        recommended_skew_ratio_bps: u32::MAX,
        required_skew_ratio_bps: u32::MAX,
        min_total_rows: 1,
        min_tombstoned_rows: 1,
    }
}

fn policy_args(enabled: bool) -> SetVectorMaintenancePolicyArgs {
    SetVectorMaintenancePolicyArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        index_id: INDEX_ID,
        enabled,
        policy: tombstone_required_policy(),
        // Degenerate def.nlist = 1 cannot be defaulted, so an explicit rebuild target is required.
        target_nlist: Some(2),
        sample_limit: 16,
        scan_max_pages: 8,
        rebuild_max_subjects: 4,
        cleanup_max_work: 8,
    }
}

fn set_policy(
    env: &FederationEnv,
    args: &SetVectorMaintenancePolicyArgs,
) -> Result<(), RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_vector_maintenance_policy",
            Encode!(args).expect("encode set policy"),
        )
        .expect("admin_set_vector_maintenance_policy call");
    Decode!(&bytes, Result<(), RouterError>).expect("decode set policy")
}

fn maintenance_step(
    env: &FederationEnv,
    sender: Principal,
) -> Result<VectorMaintenanceStepOutcome, RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            sender,
            "admin_vector_maintenance_step",
            Encode!(&GRAPH_NAME.to_string(), &INDEX_ID).expect("encode step args"),
        )
        .expect("admin_vector_maintenance_step call");
    Decode!(&bytes, Result<VectorMaintenanceStepOutcome, RouterError>).expect("decode step outcome")
}

fn maintenance_status(env: &FederationEnv) -> VectorMaintenanceStatusView {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "vector_maintenance_status",
            Encode!(&GRAPH_NAME.to_string(), &INDEX_ID).expect("encode status args"),
        )
        .expect("vector_maintenance_status call");
    Decode!(&bytes, Result<VectorMaintenanceStatusView, RouterError>)
        .expect("decode status")
        .expect("status ok")
}

fn publish(env: &FederationEnv) -> Result<(), RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_publish_vector_rebuild",
            Encode!(&GRAPH_NAME.to_string(), &INDEX_ID).expect("encode publish args"),
        )
        .expect("admin_publish_vector_rebuild call");
    Decode!(&bytes, Result<(), RouterError>).expect("decode publish")
}

fn reset(env: &FederationEnv) -> Result<(), RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_vector_maintenance_reset",
            Encode!(&GRAPH_NAME.to_string(), &INDEX_ID).expect("encode reset args"),
        )
        .expect("admin_vector_maintenance_reset call");
    Decode!(&bytes, Result<(), RouterError>).expect("decode reset")
}

fn router_vector_search(env: &FederationEnv, query_value: f32, top_k: u32) -> VectorSearchResult {
    use gleaph_router::types::RouterVectorSearchRequest;
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
            Encode!(&req).expect("encode search"),
        )
        .expect("vector_search call");
    Decode!(&bytes, Result<VectorSearchResult, RouterError>)
        .expect("decode search")
        .expect("search ok")
}

#[test]
fn router_push_drives_to_awaiting_publish_then_explicit_publish() {
    let env = install_federation();
    let _vector = ready_activated_vector_with_tombstone(&env);

    // Disabled by default: with no policy yet, the push step is a clean no-op.
    assert_eq!(
        maintenance_step(&env, env.admin).expect("step"),
        VectorMaintenanceStepOutcome::Disabled,
        "no policy -> Disabled no-op"
    );

    set_policy(&env, &policy_args(true)).expect("enable policy");

    // Drive one bounded unit per call until the rebuild reaches ReadyToPublish. The run must stop
    // there (publish is explicit) and must pass through scan -> RebuildStarted(Required) -> drive.
    let mut saw_rebuild_started = false;
    let mut awaiting_publish = false;
    for _ in 0..MAX_STEPS {
        match maintenance_step(&env, env.admin).expect("step") {
            VectorMaintenanceStepOutcome::Disabled => panic!("policy is enabled"),
            VectorMaintenanceStepOutcome::Stepped(result) => match result {
                VectorMaintenanceStepResult::Scanning { .. }
                | VectorMaintenanceStepResult::RebuildAdvanced(_) => {}
                VectorMaintenanceStepResult::RebuildStarted(rec) => {
                    assert_eq!(rec, VectorMaintenanceRecommendation::RebuildRequired);
                    saw_rebuild_started = true;
                }
                VectorMaintenanceStepResult::AwaitingPublish(status) => {
                    assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
                    awaiting_publish = true;
                    break;
                }
                other => panic!("unexpected pre-publish outcome: {other:?}"),
            },
        }
    }
    assert!(
        saw_rebuild_started,
        "a required recommendation started a rebuild"
    );
    assert!(awaiting_publish, "the run stopped at ReadyToPublish");

    // The push step is idempotent at ReadyToPublish: it keeps returning AwaitingPublish, never
    // auto-publishing.
    assert!(matches!(
        maintenance_step(&env, env.admin).expect("step"),
        VectorMaintenanceStepOutcome::Stepped(VectorMaintenanceStepResult::AwaitingPublish(_))
    ));

    // Search still returns the active (pre-publish) generation's hits.
    assert_eq!(router_vector_search(&env, 9.0, 10).hits.len(), 4);

    // Explicit publish flips the active version; search keeps returning hits afterward.
    publish(&env).expect("publish");
    let hits = router_vector_search(&env, 9.0, 10).hits;
    assert!(!hits.is_empty(), "search returns hits after publish");

    // Continue pushing: cleanup drains and a fresh scan finds the compacted index healthy.
    let mut reached_healthy = false;
    for _ in 0..MAX_STEPS {
        match maintenance_step(&env, env.admin).expect("step") {
            VectorMaintenanceStepOutcome::Stepped(VectorMaintenanceStepResult::Healthy) => {
                reached_healthy = true;
                break;
            }
            VectorMaintenanceStepOutcome::Stepped(_) => {}
            VectorMaintenanceStepOutcome::Disabled => panic!("policy still enabled"),
        }
    }
    assert!(reached_healthy, "the compacted index is judged healthy");
}

#[test]
fn disabled_policy_is_noop_and_rbac_enforced() {
    let env = install_federation();
    let _vector = ready_activated_vector_with_tombstone(&env);

    // A stored-but-disabled policy is a no-op (distinct from absent).
    set_policy(&env, &policy_args(false)).expect("store disabled policy");
    assert_eq!(
        maintenance_step(&env, env.admin).expect("step"),
        VectorMaintenanceStepOutcome::Disabled,
    );

    // RBAC: a non-admin caller is forbidden from stepping.
    let stranger = Principal::from_slice(&[0x42; 29]);
    assert!(matches!(
        maintenance_step(&env, stranger),
        Err(RouterError::Forbidden)
    ));
}

#[test]
fn maintenance_state_survives_upgrade_and_reset_recovers() {
    let env = install_federation();
    let vector = ready_activated_vector_with_tombstone(&env);
    set_policy(&env, &policy_args(true)).expect("enable policy");

    // One push starts and exhausts the single-page scan: in-progress execution state now exists.
    assert!(matches!(
        maintenance_step(&env, env.admin).expect("step"),
        VectorMaintenanceStepOutcome::Stepped(VectorMaintenanceStepResult::Scanning { .. })
    ));
    assert!(
        matches!(
            maintenance_status(&env).maintenance_state,
            Some(VectorMaintenanceStateView::Scanning { .. })
        ),
        "execution state is Scanning before upgrade"
    );

    // Upgrade the vector canister: the stable maintenance region must persist the scan state (it is
    // not heap-only like the centroid cache).
    env.pic
        .upgrade_canister(
            vector,
            gleaph_pocket_ic_tests::wasm_bytes("VECTOR_INDEX_WASM"),
            Encode!().expect("encode empty upgrade arg"),
            None,
        )
        .expect("vector upgrade");
    assert!(
        matches!(
            maintenance_status(&env).maintenance_state,
            Some(VectorMaintenanceStateView::Scanning { .. })
        ),
        "Scanning execution state survives upgrade"
    );

    // Reset returns the (forwarded) vector-canister execution state to Idle and maintenance resumes.
    reset(&env).expect("reset");
    assert!(matches!(
        maintenance_status(&env).maintenance_state,
        Some(VectorMaintenanceStateView::Idle)
    ));
    assert!(matches!(
        maintenance_step(&env, env.admin).expect("step"),
        VectorMaintenanceStepOutcome::Stepped(VectorMaintenanceStepResult::Scanning { .. })
    ));
}
