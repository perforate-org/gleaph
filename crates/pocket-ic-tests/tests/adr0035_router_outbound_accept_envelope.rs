//! PocketIC E2E for ADR 0035 Slices 5 and 6: Router outbound accept_envelope send and
//! symmetric Provision -> Router `router_ack` callback.
//!
//! One fresh PocketIC instance, six named scenarios:
//!   1. install + bootstrap: install Router + Provision with a bootstrap binding that
//!      authorizes the Router principal.
//!   2. router outbound fresh admission: call `provision_graph` as the Router admin and
//!      assert an `Accepted` response, then a second identical call asserts `Replay`.
//!   3. post-upgrade durable binding: upgrade the Router with `provision_canister: None`
//!      and assert the durable stable binding still routes the next outbound call.
//!   4. provision -> router ack: call `router_ack` as the Provision canister principal with
//!      `accepted_registry_version=7` and assert `Ok(RouterAckResponse { accepted_registry_version: 7 })`.
//!   5. router ack idempotent replay: repeat the same ack and assert `Ok` with the same version.
//!   6. router ack version conflict: call `router_ack` with `accepted_registry_version=8` and
//!      assert `Err(AckConflict { stored: 7 })`.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::provisioning::ProvisionableResourceKind;
use gleaph_graph_kernel::provisioning::wire::{
    ProvisionJobSummary, ProvisionableResource, RouterAckResponse, RouterProvisionAck,
};
use gleaph_pocket_ic_tests::{install_provision_canister, new_pocket_ic, wasm_bytes};
use gleaph_provision::types::{
    AdminInstallDeploymentBindingArgs, AdminInstallError, ArtifactAuditAction,
    ArtifactAuditOutcome, ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs,
    BootstrapAuthAction, BootstrapAuthEntry, CanisterKind, DeploymentBinding, ReleaseActivateArgs,
    ReleaseInstallArgs, ReleasePublishArgs,
};
use gleaph_router::RouterInitArgs;
use gleaph_router::types::{ProvisionGraphArgs, ProvisionGraphResponse};

struct Env {
    pic: pocket_ic::PocketIc,
    admin: Principal,
    router: Principal,
    provision: Principal,
}

fn install_router_and_provision() -> Env {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = pic.create_canister();
    pic.add_cycles(router, 2_000_000_000_000);

    // Install Provision canister first so we know its principal for Router init.
    let binding = DeploymentBinding {
        deployment_id: "deploy-p0058".to_owned(),
        router_principal: router,
        governance_principal: admin,
        binding_version: 1,
    };
    let provision = install_provision_canister(&pic, binding);

    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: Some(provision),
        })
        .expect("encode router init"),
        None,
    );

    Env {
        pic,
        admin,
        router,
        provision,
    }
}

fn call_provision_graph(
    env: &Env,
    args: &ProvisionGraphArgs,
) -> Result<ProvisionGraphResponse, gleaph_graph_kernel::federation::RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "provision_graph",
            Encode!(args).expect("encode provision_graph"),
        )
        .unwrap_or_else(|e| panic!("provision_graph on router: {e:?}"));

    Decode!(
        &bytes,
        Result<ProvisionGraphResponse, gleaph_graph_kernel::federation::RouterError>
    )
    .expect("decode provision_graph response")
}

fn call_router_ack(
    env: &Env,
    ack: &RouterProvisionAck,
) -> Result<RouterAckResponse, gleaph_graph_kernel::federation::RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.provision,
            "router_ack",
            Encode!(ack).expect("encode router_ack"),
        )
        .unwrap_or_else(|e| panic!("router_ack on router: {e:?}"));

    Decode!(
        &bytes,
        Result<RouterAckResponse, gleaph_graph_kernel::federation::RouterError>
    )
    .expect("decode router_ack response")
}

fn call_admin_install(
    env: &Env,
    caller: Principal,
    args: &AdminInstallDeploymentBindingArgs,
) -> Result<BootstrapAuthEntry, AdminInstallError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "admin_install_deployment_binding",
            Encode!(args).expect("encode admin_install_deployment_binding"),
        )
        .unwrap_or_else(|e| panic!("admin_install_deployment_binding on provision: {e:?}"));

    Decode!(
        &bytes,
        Result<BootstrapAuthEntry, AdminInstallError>
    )
    .expect("decode admin_install_deployment_binding response")
}

#[test]
fn router_outbound_accept_envelope_fresh_admission_replay_and_upgrade_durability() {
    let env = install_router_and_provision();
    let _ = env.provision;

    let args = ProvisionGraphArgs {
        deployment_id: "deploy-p0058".to_owned(),
        request_fingerprint: "fp-fresh-1".to_owned(),
        graph_name: "p0058.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-0".to_owned(),
        }],
        authorized_caller: env.admin,
        release_id: "rel-1".to_owned(),
    };

    // Scenario 1: fresh admission returns Accepted.
    let first = call_provision_graph(&env, &args).expect("first provision_graph accepted");
    let (state, intent_lock_count) = match first {
        ProvisionGraphResponse::Accepted {
            job_view: ProvisionJobSummary { state, .. },
            intent_lock_count,
        } => (state, intent_lock_count),
        ProvisionGraphResponse::Replay { .. } => panic!("first call must be Accepted"),
        ProvisionGraphResponse::Completed { .. } => panic!("first call must not be Completed"),
    };
    assert_eq!(
        state, "Reserved",
        "fresh admission reserves the intent lock before returning"
    );
    assert_eq!(intent_lock_count, 1, "one intent lock for one resource");

    // Scenario 2: identical retry returns Replay.
    let second = call_provision_graph(&env, &args).expect("second provision_graph accepted");
    assert!(
        matches!(second, ProvisionGraphResponse::Replay { .. }),
        "second call must be Replay"
    );

    // Scenario 4: Provision -> Router ack with accepted_registry_version=7.
    let ack = RouterProvisionAck {
        deployment_id: "deploy-p0058".to_owned(),
        request_id: "p0058.graph-fp-fresh-1".to_owned(),
        accepted_registry_version: 7,
    };
    let ack_response = call_router_ack(&env, &ack).expect("router_ack accepted");
    assert_eq!(
        ack_response.accepted_registry_version, 7,
        "router_ack returns the accepted registry version"
    );

    // Scenario 5: idempotent replay returns the same version.
    let ack_replay = call_router_ack(&env, &ack).expect("router_ack replay accepted");
    assert_eq!(
        ack_replay.accepted_registry_version, 7,
        "router_ack replay returns the stored registry version"
    );

    // Scenario 6: differing registry version returns AckConflict.
    let bad_ack = RouterProvisionAck {
        deployment_id: "deploy-p0058".to_owned(),
        request_id: "p0058.graph-fp-fresh-1".to_owned(),
        accepted_registry_version: 8,
    };
    let conflict =
        call_router_ack(&env, &bad_ack).expect_err("router_ack must conflict on version mismatch");
    assert_eq!(
        conflict,
        gleaph_graph_kernel::federation::RouterError::AckConflict { stored: 7 },
        "router_ack conflict must report the stored version"
    );

    // Scenario 7: admin_install_deployment_binding succeeds when called as the bootstrap
    // governance principal seeded at init; a follow-up Router outbound call for the new
    // deployment is no longer rejected with UnknownDeployment.
    let admin_install_args = AdminInstallDeploymentBindingArgs {
        deployment_id: "deploy-admin-1".to_owned(),
        router_principal: env.router,
        governance_principal: env.admin,
        binding_version: 2,
    };
    let admin_install_result = call_admin_install(&env, env.admin, &admin_install_args)
        .expect("bootstrap governance admin_install must succeed");
    assert_eq!(
        admin_install_result.action,
        BootstrapAuthAction::AdminInstall
    );
    assert_eq!(admin_install_result.caller, env.admin);

    let admin_installed_args = ProvisionGraphArgs {
        deployment_id: "deploy-admin-1".to_owned(),
        request_fingerprint: "fp-admin-1".to_owned(),
        graph_name: "admin1.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-admin-1".to_owned(),
        }],
        authorized_caller: env.admin,
        release_id: "rel-admin-1".to_owned(),
    };
    let admin_installed = call_provision_graph(&env, &admin_installed_args)
        .expect("outbound call for admin-installed deployment must be accepted");
    assert!(
        matches!(admin_installed, ProvisionGraphResponse::Accepted { .. }),
        "admin-installed deployment must accept fresh admission"
    );

    // Scenario 8: admin_install as a non-bootstrap, non-stored principal against a missing
    // deployment returns UnknownDeployment and does not install a binding.
    let wrong_principal = Principal::from_slice(&[0xCD; 29]);
    let missing_install_args = AdminInstallDeploymentBindingArgs {
        deployment_id: "deploy-admin-missing".to_owned(),
        router_principal: env.router,
        governance_principal: wrong_principal,
        binding_version: 3,
    };
    let reject = call_admin_install(&env, wrong_principal, &missing_install_args)
        .expect_err("unauthorized admin_install must be rejected");
    assert_eq!(
        reject,
        AdminInstallError::UnknownDeployment("deploy-admin-missing".to_owned())
    );

    let missing_args = ProvisionGraphArgs {
        deployment_id: "deploy-admin-missing".to_owned(),
        request_fingerprint: "fp-missing-1".to_owned(),
        graph_name: "missing.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-missing-1".to_owned(),
        }],
        authorized_caller: env.admin,
        release_id: "rel-missing-1".to_owned(),
    };
    let missing_result = call_provision_graph(&env, &missing_args)
        .expect_err("outbound call for rejected deployment must still be rejected");
    assert!(
        matches!(
            missing_result,
            gleaph_graph_kernel::federation::RouterError::UnknownDeployment(_)
        ),
        "expected UnknownDeployment, got {missing_result:?}"
    );

    // Scenario 3: upgrade the Router with empty args; the durable stable
    // binding must keep the outbound path reachable (ADR 0039).
    env.pic
        .upgrade_canister(
            env.router,
            wasm_bytes("ROUTER_WASM"),
            Encode!(&()).expect("encode empty router upgrade args"),
            None,
        )
        .expect("upgrade router canister");

    let post_args = ProvisionGraphArgs {
        request_fingerprint: "fp-post-upgrade-1".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-1".to_owned(),
        }],
        ..args
    };
    let third =
        call_provision_graph(&env, &post_args).expect("post-upgrade provision_graph accepted");
    assert!(
        matches!(third, ProvisionGraphResponse::Accepted { .. }),
        "post-upgrade call must still reach the original Provision canister"
    );
}

#[allow(clippy::result_large_err)]
fn call_artifact_publish(
    env: &Env,
    caller: Principal,
    args: &ArtifactPublishMetadataArgs,
) -> Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "artifact_publish_metadata",
            Encode!(args).expect("encode artifact_publish_metadata"),
        )
        .unwrap_or_else(|e| panic!("artifact_publish_metadata on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError>
    )
    .expect("decode artifact_publish_metadata response")
}

#[allow(clippy::result_large_err)]
fn call_artifact_upload(
    env: &Env,
    caller: Principal,
    args: &ArtifactUploadChunkArgs,
) -> Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "artifact_upload_chunk",
            Encode!(args).expect("encode artifact_upload_chunk"),
        )
        .unwrap_or_else(|e| panic!("artifact_upload_chunk on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError>
    )
    .expect("decode artifact_upload_chunk response")
}

fn call_release_publish(
    env: &Env,
    caller: Principal,
    args: &ReleasePublishArgs,
) -> Result<gleaph_provision::types::ReleaseManifest, gleaph_provision::types::ReleaseError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "release_publish",
            Encode!(args).expect("encode release_publish"),
        )
        .unwrap_or_else(|e| panic!("release_publish on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<gleaph_provision::types::ReleaseManifest, gleaph_provision::types::ReleaseError>
    )
    .expect("decode release_publish response")
}

fn call_release_activate(
    env: &Env,
    caller: Principal,
    args: &ReleaseActivateArgs,
) -> Result<gleaph_provision::types::ReleaseActivateResult, gleaph_provision::types::ReleaseError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "release_activate",
            Encode!(args).expect("encode release_activate"),
        )
        .unwrap_or_else(|e| panic!("release_activate on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<gleaph_provision::types::ReleaseActivateResult, gleaph_provision::types::ReleaseError>
    )
    .expect("decode release_activate response")
}

fn call_release_install(
    env: &Env,
    caller: Principal,
    args: &ReleaseInstallArgs,
) -> Result<gleaph_provision::types::ReleaseInstallResult, gleaph_provision::types::InstallError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "release_install",
            Encode!(args).expect("encode release_install"),
        )
        .unwrap_or_else(|e| panic!("release_install on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<gleaph_provision::types::ReleaseInstallResult, gleaph_provision::types::InstallError>
    )
    .expect("decode release_install response")
}

#[allow(clippy::result_large_err)]
fn call_release_get_active(
    env: &Env,
    caller: Principal,
) -> Option<gleaph_provision::types::ReleaseActivateResult> {
    let bytes = env
        .pic
        .query_call(
            env.provision,
            caller,
            "release_get_active",
            Encode!().expect("encode release_get_active"),
        )
        .unwrap_or_else(|e| panic!("release_get_active on provision: {e:?}"));
    Decode!(
        &bytes,
        Option<gleaph_provision::types::ReleaseActivateResult>
    )
    .expect("decode release_get_active response")
}
#[allow(clippy::result_large_err)]
fn call_artifact_audit_history(
    env: &Env,
    caller: Principal,
) -> Result<Vec<gleaph_provision::types::ArtifactAuditEntry>, gleaph_provision::types::ArtifactError>
{
    let bytes = env
        .pic
        .query_call(
            env.provision,
            caller,
            "artifact_audit_history",
            Encode!().expect("encode artifact_audit_history"),
        )
        .unwrap_or_else(|e| panic!("artifact_audit_history on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<
            Vec<gleaph_provision::types::ArtifactAuditEntry>,
            gleaph_provision::types::ArtifactError,
        >
    )
    .expect("decode artifact_audit_history response")
}

fn publish_verified_artifact(
    env: &Env,
    kind: CanisterKind,
    version: &str,
    chunks: Vec<&[u8]>,
) -> gleaph_provision::types::ArtifactId {
    let full: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let full_sha = gleaph_provision::types::sha256(&full);
    let chunk_hashes: Vec<[u8; 32]> = chunks
        .iter()
        .map(|c| gleaph_provision::types::sha256(c))
        .collect();
    let id = gleaph_provision::types::ArtifactId::new(kind.clone(), version.to_owned(), full_sha);

    call_artifact_publish(
        env,
        env.admin,
        &ArtifactPublishMetadataArgs {
            canister_kind: kind,
            semantic_version: version.to_owned(),
            sha256: full_sha,
            byte_length: full.len() as u64,
            chunk_hashes: chunk_hashes.clone(),
        },
    )
    .expect("publish artifact");

    for (i, chunk) in chunks.iter().enumerate() {
        call_artifact_upload(
            env,
            env.admin,
            &ArtifactUploadChunkArgs {
                artifact_id: id.clone(),
                chunk_index: i as u32,
                bytes: chunk.to_vec(),
            },
        )
        .expect("upload artifact chunk");
    }
    id
}

/// Scenario 9: artifact publish + upload chunks succeeds and writes audit entries.
#[test]
fn artifact_publish_and_upload_chunks_succeeds() {
    let env = install_router_and_provision();
    let id = publish_verified_artifact(
        &env,
        CanisterKind::Router,
        "0.1.0",
        vec![b"router-chunk-0", b"router-chunk-1"],
    );

    let history =
        call_artifact_audit_history(&env, env.admin).expect("audit history query must succeed");
    let publish = history
        .iter()
        .find(|e| e.action == ArtifactAuditAction::PublishArtifact)
        .expect("PublishArtifact audit entry");
    assert_eq!(publish.outcome, ArtifactAuditOutcome::Success);
    assert_eq!(publish.artifact_id.as_ref().unwrap().sha256, id.sha256);

    let upload = history
        .iter()
        .find(|e| e.action == ArtifactAuditAction::UploadChunk)
        .expect("UploadChunk audit entry");
    assert_eq!(upload.outcome, ArtifactAuditOutcome::Success);

    let verify = history
        .iter()
        .find(|e| e.action == ArtifactAuditAction::VerifyArtifact)
        .expect("VerifyArtifact audit entry");
    assert_eq!(verify.outcome, ArtifactAuditOutcome::Success);
}

/// Scenario 10: release publish succeeds and writes a PublishRelease audit entry.
#[test]
fn release_publish_succeeds() {
    let env = install_router_and_provision();
    let ids = vec![
        publish_verified_artifact(&env, CanisterKind::Router, "0.1.0", vec![b"r0"]),
        publish_verified_artifact(&env, CanisterKind::Graph, "0.1.0", vec![b"g0"]),
        publish_verified_artifact(&env, CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]),
        publish_verified_artifact(&env, CanisterKind::VectorIndex, "0.1.0", vec![b"v0"]),
    ];

    let release_id = gleaph_provision::types::ReleaseId("release-pocket-10".to_owned());
    call_release_publish(
        &env,
        env.admin,
        &ReleasePublishArgs {
            release_id: release_id.clone(),
            artifact_ids: ids,
        },
    )
    .expect("release_publish must succeed");

    let history =
        call_artifact_audit_history(&env, env.admin).expect("audit history query must succeed");
    let publish = history
        .iter()
        .find(|e| e.action == ArtifactAuditAction::PublishRelease)
        .expect("PublishRelease audit entry");
    assert_eq!(publish.outcome, ArtifactAuditOutcome::Success);
    assert_eq!(publish.release_id.as_ref().unwrap().0, release_id.0);
}

/// Scenario 11: release activate succeeds and writes an ActivateRelease audit entry.
#[test]
fn release_activate_succeeds() {
    let env = install_router_and_provision();
    let ids = vec![
        publish_verified_artifact(&env, CanisterKind::Router, "0.1.0", vec![b"r0"]),
        publish_verified_artifact(&env, CanisterKind::Graph, "0.1.0", vec![b"g0"]),
        publish_verified_artifact(&env, CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]),
        publish_verified_artifact(&env, CanisterKind::VectorIndex, "0.1.0", vec![b"v0"]),
    ];
    let release_id = gleaph_provision::types::ReleaseId("release-pocket-11".to_owned());
    call_release_publish(
        &env,
        env.admin,
        &ReleasePublishArgs {
            release_id: release_id.clone(),
            artifact_ids: ids,
        },
    )
    .expect("release_publish must succeed");

    let active = call_release_activate(
        &env,
        env.admin,
        &ReleaseActivateArgs {
            release_id: release_id.clone(),
        },
    )
    .expect("release_activate must succeed");
    assert_eq!(active.release_id, release_id);
    assert_eq!(active.previous_release_id, None);

    let current = call_release_get_active(&env, env.admin)
        .expect("release_get_active must return active release");
    assert_eq!(current.release_id, release_id);

    let history =
        call_artifact_audit_history(&env, env.admin).expect("audit history query must succeed");
    let activate = history
        .iter()
        .find(|e| e.action == ArtifactAuditAction::ActivateRelease)
        .expect("ActivateRelease audit entry");
    assert_eq!(activate.outcome, ArtifactAuditOutcome::Success);
    assert_eq!(activate.release_id.as_ref().unwrap().0, release_id.0);
}

/// A tiny valid WebAssembly module with `canister_init` and `memory` exports,
/// suitable for a real `install_chunked_code` call in PocketIC without exceeding the
/// 1 MiB chunk budget.
fn minimal_canister_wasm() -> Vec<u8> {
    vec![
        0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, // magic + version
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section: () -> ()
        0x03, 0x02, 0x01, 0x00, // function section
        0x05, 0x03, 0x01, 0x00, 0x01, // memory section: 1 page
        0x07, 0x1A, // export section
        0x02, // 2 exports
        0x0D, 0x63, 0x61, 0x6E, 0x69, 0x73, 0x74, 0x65, 0x72, 0x5F, 0x69, 0x6E, 0x69, 0x74, 0x00,
        0x00, // func export "canister_init"
        0x06, 0x6D, 0x65, 0x6D, 0x6F, 0x72, 0x79, 0x02, 0x00, // memory export "memory"
        0x0A, 0x04, 0x01, 0x02, 0x00, 0x0B, // code section
    ]
}

#[allow(clippy::result_large_err)]
fn publish_valid_artifact(
    env: &Env,
    kind: CanisterKind,
    version: &str,
) -> gleaph_provision::types::ArtifactId {
    let wasm = minimal_canister_wasm();
    let full_sha = gleaph_provision::types::sha256(&wasm);
    let chunk_hash = gleaph_provision::types::sha256(&wasm);
    let id = gleaph_provision::types::ArtifactId::new(kind.clone(), version.to_owned(), full_sha);
    call_artifact_publish(
        env,
        env.admin,
        &ArtifactPublishMetadataArgs {
            canister_kind: kind,
            semantic_version: version.to_owned(),
            sha256: full_sha,
            byte_length: wasm.len() as u64,
            chunk_hashes: vec![chunk_hash],
        },
    )
    .expect("artifact_publish_metadata");
    call_artifact_upload(
        env,
        env.admin,
        &ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 0,
            bytes: wasm,
        },
    )
    .expect("artifact_upload_chunk");
    id
}

/// Scenario 12: release install succeeds against a real management canister.
#[test]
fn release_install_succeeds() {
    let env = install_router_and_provision();
    let ids = vec![
        publish_valid_artifact(&env, CanisterKind::Router, "1.0.0"),
        publish_valid_artifact(&env, CanisterKind::Graph, "1.0.0"),
        publish_valid_artifact(&env, CanisterKind::PropertyIndex, "1.0.0"),
        publish_valid_artifact(&env, CanisterKind::VectorIndex, "1.0.0"),
    ];
    let release_id = gleaph_provision::types::ReleaseId("release-pocket-12".to_owned());
    call_release_publish(
        &env,
        env.admin,
        &ReleasePublishArgs {
            release_id: release_id.clone(),
            artifact_ids: ids,
        },
    )
    .expect("release_publish");
    call_release_activate(
        &env,
        env.admin,
        &ReleaseActivateArgs {
            release_id: release_id.clone(),
        },
    )
    .expect("release_activate");

    let target = env.pic.create_canister();
    env.pic.add_cycles(target, 2_000_000_000_000);
    env.pic
        .set_controllers(target, None, vec![env.admin, env.provision])
        .expect("set target controllers");

    let result = call_release_install(
        &env,
        env.admin,
        &ReleaseInstallArgs {
            target_canister_kind: CanisterKind::Router,
            target_canister_id: Some(target),
            install_args: vec![],
            registry_version: 1,
        },
    )
    .expect("release_install must succeed");
    assert_eq!(result.release_id, release_id);
    assert_eq!(result.target_canister_id, target);
    assert_eq!(result.installed_chunks, 1);

    let history =
        call_artifact_audit_history(&env, env.admin).expect("audit history query must succeed");
    let install = history
        .iter()
        .find(|e| e.action == ArtifactAuditAction::InstallRelease)
        .expect("InstallRelease audit entry");
    assert_eq!(install.outcome, ArtifactAuditOutcome::Success);
    assert_eq!(install.target_canister, Some(target));
}
