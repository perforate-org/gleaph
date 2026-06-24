//! PocketIC coverage for the ADR 0031 Slice 3 Router vector-index catalog, target resolution, and
//! the fail-closed activation gate.
//!
//! Slice 3 makes vector dispatch addressable from the Router (register by embedding **name**, set a
//! single target, list, inspect activation status / resolve target) while keeping production
//! dispatch and backfill **fail-closed**: `incarnation_fencing_enabled()` is `const false`, so a
//! targeted definition terminates at `DispatchBlockedMissingIncarnationFence` and the backfill admin
//! surface returns `VectorDispatchActivationBlocked { MissingEmbeddingIncarnationFence }`.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::{RouterError, VectorActivationBlockReason};
use gleaph_pocket_ic_tests::{FederationEnv, GRAPH_NAME, install_federation};
use gleaph_router::types::{
    AdminVectorIndexBackfillStepArgs, RegisterVectorIndexArgs, SetVectorIndexTargetArgs,
    VectorIndexActivationStateView, VectorIndexActivationStatus, VectorIndexInfo,
};

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

fn backfill_step(env: &FederationEnv, index_id: u32) -> Result<(), RouterError> {
    let args = AdminVectorIndexBackfillStepArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        index_id,
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
    Decode!(&bytes, Result<(), RouterError>).expect("decode backfill result")
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

    // A targeted definition is blocked by the missing incarnation fence, with an explained reason.
    let status = activation_status(&env, INDEX_ID).expect("activation status");
    assert_eq!(
        status.activation_state,
        VectorIndexActivationStateView::DispatchBlockedMissingIncarnationFence,
        "fail-closed: a targeted def can never reach DispatchEnabled in Slice 3"
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

    // The backfill admin surface fails closed for production.
    assert!(
        matches!(
            backfill_step(&env, INDEX_ID),
            Err(RouterError::VectorDispatchActivationBlocked(
                VectorActivationBlockReason::MissingEmbeddingIncarnationFence
            ))
        ),
        "backfill must fail closed until incarnation fencing lands"
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
