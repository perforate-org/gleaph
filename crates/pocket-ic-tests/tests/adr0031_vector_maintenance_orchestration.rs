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
    VectorCanisterError, VectorEmbeddingSyncOp, VectorEncoding, VectorMaintenancePolicy,
    VectorMaintenanceRecommendation, VectorMaintenanceStepResult, VectorMetric, VectorRebuildPhase,
    VectorSearchResult, VectorSlabCompactionPhase, VectorSlabStats, VectorSubject,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, ensure_user_graph_type, install_federation, install_vector_canister,
};
use gleaph_router::types::{
    RegisterVectorIndexArgs, SetVectorMaintenancePolicyArgs, VectorMaintenanceStateView,
    VectorMaintenanceStatusView, VectorMaintenanceStepOutcome, VectorSlabCompactOutcome,
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
            "get_graph_id",
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
        labels: vec!["User".to_string()],
        metric: Some(VectorMetric::L2Squared),
        encoding: None,
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
            "set_vector_dispatch_enabled",
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
            "admin_set_vector_canister",
            Encode!(&vector).expect("encode set vector routing"),
        )
        .expect("admin_set_vector_canister call");
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
        vector_canister: vector,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "attach_vector_shard",
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
        mutation_id: version,
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
            env.graph_source,
            "vector_upsert",
            Encode!(&op).expect("encode upsert op"),
        )
        .expect("vector_upsert call");
    Decode!(&bytes, Result<(), VectorCanisterError>)
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
    policy_args_with_tier(enabled, None)
}

fn policy_args_with_tier(enabled: bool, code_tier: Option<bool>) -> SetVectorMaintenancePolicyArgs {
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
        target_fine_nlist: None,
        code_tier,
        eps_query_bps: None,
        eps_fine_bps: None,
        // Plan 0343: compaction driver stays disabled in the rebuild-orchestration fixtures.
        compact_dead_bytes_threshold: None,
        compact_max_pages: 0,
        compact_max_bytes: 0,
    }
}

fn policy_args_with_compaction(enabled: bool) -> SetVectorMaintenancePolicyArgs {
    let mut args = policy_args_with_tier(enabled, None);
    // Plan 0343: arm the compaction driver with a 1-byte threshold so any dead slab space
    // triggers; budgets mirror the small fixture scale.
    args.compact_dead_bytes_threshold = Some(1);
    args.compact_max_pages = 8;
    args.compact_max_bytes = 1 << 20;
    args
}

fn compact_step(
    env: &FederationEnv,
    sender: Principal,
) -> Result<VectorSlabCompactOutcome, RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            sender,
            "advance_vector_slab_compact",
            Encode!(&GRAPH_NAME.to_string(), &INDEX_ID).expect("encode compact args"),
        )
        .expect("advance_vector_slab_compact call");
    Decode!(&bytes, Result<VectorSlabCompactOutcome, RouterError>).expect("decode compact outcome")
}

fn slab_dead_bytes(env: &FederationEnv) -> u64 {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_vector_slab_stats",
            Encode!(&GRAPH_NAME.to_string(), &Some(INDEX_ID)).expect("encode slab stats args"),
        )
        .expect("get_vector_slab_stats call");
    Decode!(&bytes, Result<VectorSlabStats, RouterError>)
        .expect("decode slab stats")
        .expect("slab stats ok")
        .slab
        .estimated_unreferenced_bytes
}

#[test]
fn router_driven_compaction_reclaims_dead_space_with_identical_recall() {
    let env = install_federation();
    ensure_user_graph_type(&env);
    let _vector = ready_activated_vector_with_tombstone(&env);

    // Driver disarmed by default: with no policy yet, the compact advance is a clean no-op
    // (wrong-impl guard: it must not touch the canister).
    assert_eq!(
        compact_step(&env, env.admin).expect("compact step"),
        VectorSlabCompactOutcome::Disabled,
        "no policy -> Disabled no-op"
    );
    // RBAC mirrors the maintenance step: a non-admin caller is forbidden.
    let stranger = Principal::from_slice(&[0x42; 29]);
    assert!(matches!(
        compact_step(&env, stranger),
        Err(RouterError::Forbidden)
    ));

    set_policy(&env, &policy_args_with_compaction(true)).expect("enable policy");

    // Drive the rebuild path to publish, then keep pushing until cleanup drains and the fresh
    // scan judges the index healthy: the superseded generation's pages are now dead slab space.
    let mut awaiting_publish = false;
    for _ in 0..MAX_STEPS {
        match maintenance_step(&env, env.admin).expect("step") {
            VectorMaintenanceStepOutcome::Stepped(
                VectorMaintenanceStepResult::AwaitingPublish(_),
            ) => {
                awaiting_publish = true;
                break;
            }
            VectorMaintenanceStepOutcome::Stepped(_) => {}
            VectorMaintenanceStepOutcome::Disabled => panic!("policy is enabled"),
        }
    }
    assert!(awaiting_publish, "rebuild reached ReadyToPublish");
    publish(&env).expect("publish");
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
    assert!(reached_healthy, "cleanup drained after publish");

    // The fixture must actually contain dead space, or the trigger below would be vacuous.
    let dead_before = slab_dead_bytes(&env);
    assert!(
        dead_before > 0,
        "superseded generation leaves dead slab space"
    );
    let before: Vec<(VectorSubject, f32)> = router_vector_search(&env, 9.0, 10)
        .hits
        .into_iter()
        .map(|h| (h.subject, h.distance))
        .collect();
    assert!(!before.is_empty(), "search returns hits before compaction");

    // Drive the Router advance to finalize: start (dead >= 1-byte threshold) + bounded steps.
    let mut saw_finalize = false;
    for _ in 0..MAX_STEPS {
        match compact_step(&env, env.admin).expect("compact step") {
            VectorSlabCompactOutcome::Disabled => panic!("compaction is armed"),
            VectorSlabCompactOutcome::BelowThreshold { .. } => {
                panic!("dead space must trigger the armed driver")
            }
            VectorSlabCompactOutcome::Advanced(status) => {
                if status.phase == VectorSlabCompactionPhase::Idle {
                    assert!(status.pages_moved > 0, "finalize moved at least one page");
                    saw_finalize = true;
                    break;
                }
            }
        }
    }
    assert!(
        saw_finalize,
        "compaction finalized through the Router driver"
    );

    // Tail rewound: the next advance observes zero dead bytes and starts nothing.
    assert_eq!(
        compact_step(&env, env.admin).expect("compact step"),
        VectorSlabCompactOutcome::BelowThreshold {
            estimated_unreferenced_bytes: 0
        },
        "no dead space remains after finalize"
    );

    // Recall-invariant: identical ordering and distances after the page moves.
    let after: Vec<(VectorSubject, f32)> = router_vector_search(&env, 9.0, 10)
        .hits
        .into_iter()
        .map(|h| (h.subject, h.distance))
        .collect();
    assert_eq!(after, before, "compaction preserves search recall");
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
            "set_vector_maintenance_policy",
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
            "advance_vector_maintenance",
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
            "get_vector_maintenance_status",
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
            "publish_vector_rebuild",
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
            "reset_vector_maintenance",
            Encode!(&GRAPH_NAME.to_string(), &INDEX_ID).expect("encode reset args"),
        )
        .expect("admin_vector_maintenance_reset call");
    Decode!(&bytes, Result<(), RouterError>).expect("decode reset")
}

fn router_vector_search(env: &FederationEnv, query_value: f32, top_k: u32) -> VectorSearchResult {
    let query = vec_bytes(query_value);
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "vector_search",
            Encode!(
                &GRAPH_NAME.to_string(),
                &EMBEDDING_NAME.to_string(),
                &query,
                &top_k
            )
            .expect("encode search"),
        )
        .expect("vector_search call");
    Decode!(&bytes, Result<VectorSearchResult, RouterError>)
        .expect("decode search")
        .expect("search ok")
}

#[test]
fn router_push_drives_to_awaiting_publish_then_explicit_publish() {
    let env = install_federation();
    ensure_user_graph_type(&env);
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
fn tier_on_rebuild_preserves_search_ordering_and_distances() {
    // Tier recall-invariance contract (ADR 0079 + ADR 0093): a tier-on generation must return the
    // identical hit ordering and exact distances as the tier-off generation over the same rows
    // (Stage B exact rerank). Both runs rebuild (nlist 2) and publish the same seeded rows; the
    // only difference is the policy `code_tier` flag. Path divergence itself is proven by the
    // tier-on/off canbench pair (24.14M vs 45.72M ins); this test locks the recall half.
    fn published_hits(code_tier: Option<bool>) -> Vec<(VectorSubject, f32)> {
        let env = install_federation();
        ensure_user_graph_type(&env);
        let _vector = ready_activated_vector_with_tombstone(&env);
        set_policy(&env, &policy_args_with_tier(true, code_tier)).expect("enable policy");
        let mut awaiting_publish = false;
        for _ in 0..MAX_STEPS {
            match maintenance_step(&env, env.admin).expect("step") {
                VectorMaintenanceStepOutcome::Stepped(
                    VectorMaintenanceStepResult::AwaitingPublish(status),
                ) => {
                    assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
                    awaiting_publish = true;
                    break;
                }
                VectorMaintenanceStepOutcome::Stepped(_) => {}
                VectorMaintenanceStepOutcome::Disabled => panic!("policy is enabled"),
            }
        }
        assert!(awaiting_publish, "rebuild reached ReadyToPublish");
        publish(&env).expect("publish");
        router_vector_search(&env, 9.0, 10)
            .hits
            .into_iter()
            .map(|h| (h.subject, h.distance))
            .collect()
    }

    // `Some(false)` keeps the tier-off generation explicit: a bare `None` now resolves to the
    // Router tier-on default at snapshot time, which would make this comparison vacuous.
    let off = published_hits(Some(false));
    let on = published_hits(Some(true));
    assert!(!off.is_empty(), "tier-off run returns hits after publish");
    assert_eq!(
        on, off,
        "tier-on generation returns identical ordering and distances"
    );
    assert!(
        on.windows(2).all(|w| w[0].1 <= w[1].1),
        "hits stay distance-ordered under the tier-on generation"
    );
}

#[test]
fn disabled_policy_is_noop_and_rbac_enforced() {
    let env = install_federation();
    ensure_user_graph_type(&env);
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
    ensure_user_graph_type(&env);
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
