//! PocketIC E2E for ADR 0035 Router -> Provision admission and registration completion.
//!
//! One fresh PocketIC instance covers the retained admission, authorization, and upgrade scenarios
//! plus the canonical GraphShard(0) registration-ACK boundary.
//!   1. install + bootstrap: install Router + Provision with a bootstrap binding that
//!      authorizes the Router principal.
//!   2. router outbound fresh admission: call `provision_graph` as the Router admin, assert an
//!      `Accepted` response, and observe the Provision job completed by Router's versionless ACK.
//!   3. post-upgrade durable binding: upgrade the Router with `provision_canister: None`
//!      and assert the durable stable binding still routes the next outbound call.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::provisioning::LogicalResource;
use gleaph_graph_kernel::provisioning::wire::{ProvisionJobSummary, ProvisionableResource};
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

/// The content-hash request_id for the `p0058.graph` graph with a single GraphShard(0) resource.
fn expected_request_id() -> [u8; 32] {
    gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
        "p0058.graph",
        &[ProvisionableResource {
            logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
        }],
    )
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
        bootstrap_principal: None,
    };
    let provision = install_provision_canister(&pic, binding);
    pic.add_cycles(provision, 100_000_000_000_000);

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

fn query_provision_job(
    env: &Env,
    request_id: [u8; 32],
    deployment_id: &str,
) -> Option<gleaph_provision::canister::ProvisionJobView> {
    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.router,
            "query_job",
            Encode!(&request_id, &deployment_id.to_owned()).expect("encode query_job"),
        )
        .unwrap_or_else(|e| panic!("query_job on provision: {e:?}"));
    Decode!(&bytes, Option<gleaph_provision::canister::ProvisionJobView>)
        .expect("decode query_job response")
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
fn router_graph_bootstrap_registration_ack_crosses_real_candid_boundary() {
    let env = install_router_and_provision();
    activate_graph_release(&env);

    let args = ProvisionGraphArgs {
        deployment_id: "deploy-p0058".to_owned(),
        graph_name: "p0058.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
        }],
        authorized_caller: env.admin,
        release_id: "rel-1".to_owned(),
        owner: env.admin,
        admins: std::collections::BTreeSet::new(),
    };

    // Scenario 1: fresh admission returns Accepted.
    let first = call_provision_graph(&env, &args).expect("first provision_graph accepted");
    let (state, intent_lock_count, created_resources) = match first {
        ProvisionGraphResponse::Accepted {
            job_view: ProvisionJobSummary { state, .. },
            intent_lock_count,
            created_resources,
        } => (state, intent_lock_count, created_resources),
        ProvisionGraphResponse::Replay { .. } => panic!("first call must be Accepted"),
        ProvisionGraphResponse::Completed => panic!("first call must not be Completed"),
    };
    assert_eq!(
        state, "RouterRegistrationPending",
        "fresh admission reaches Router registration before returning"
    );
    assert_eq!(intent_lock_count, 1, "one intent lock for one resource");
    assert_eq!(
        created_resources.len(),
        1,
        "one GraphShard(0) was installed"
    );

    let completed_job = query_provision_job(&env, expected_request_id(), "deploy-p0058")
        .expect("Provision job exists");
    assert_eq!(completed_job.state_name, "Completed");

    // Scenario 2: identical retry observes Router's durable completion without redispatch.
    let second = call_provision_graph(&env, &args).expect("second provision_graph accepted");
    assert!(
        matches!(second, ProvisionGraphResponse::Completed),
        "second call must return durable Completed"
    );

    // Scenario 7: admin_install_deployment_binding succeeds when called as the bootstrap
    // governance principal seeded at init; a follow-up Router outbound call for the new
    // deployment is no longer rejected with UnknownDeployment.
    let admin_install_args = AdminInstallDeploymentBindingArgs {
        deployment_id: "deploy-admin-1".to_owned(),
        router_principal: env.router,
        governance_principal: env.admin,
        binding_version: 2,
        bootstrap_principal: None,
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
        graph_name: "admin1.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
        }],
        authorized_caller: env.admin,
        release_id: "rel-admin-1".to_owned(),
        owner: env.admin,
        admins: std::collections::BTreeSet::new(),
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
        bootstrap_principal: None,
    };
    let reject = call_admin_install(&env, wrong_principal, &missing_install_args)
        .expect_err("unauthorized admin_install must be rejected");
    assert_eq!(
        reject,
        AdminInstallError::UnknownDeployment("deploy-admin-missing".to_owned())
    );

    let missing_args = ProvisionGraphArgs {
        deployment_id: "deploy-admin-missing".to_owned(),
        graph_name: "missing.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
        }],
        authorized_caller: env.admin,
        release_id: "rel-missing-1".to_owned(),
        owner: env.admin,
        admins: std::collections::BTreeSet::new(),
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
        graph_name: "post-upgrade.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
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

fn activate_graph_release(env: &Env) {
    const CHUNK_SIZE: usize = 1024 * 1024;
    let graph_wasm = wasm_bytes("GRAPH_WASM");
    let graph_chunks: Vec<&[u8]> = graph_wasm.chunks(CHUNK_SIZE).collect();
    let artifact_ids = vec![
        publish_verified_artifact(env, CanisterKind::Graph, "graph-real", graph_chunks),
        publish_verified_artifact(env, CanisterKind::Router, "router-unused", vec![b"router"]),
        publish_verified_artifact(
            env,
            CanisterKind::PropertyIndex,
            "property-unused",
            vec![b"property"],
        ),
        publish_verified_artifact(
            env,
            CanisterKind::VectorCanister,
            "vector-unused",
            vec![b"vector"],
        ),
        publish_verified_artifact(
            env,
            CanisterKind::TextCanister,
            "text-unused",
            vec![b"text"],
        ),
    ];
    let release_id = gleaph_provision::types::ReleaseId("release-graph-boundary".to_owned());
    call_release_publish(
        env,
        env.admin,
        &ReleasePublishArgs {
            release_id: release_id.clone(),
            artifact_ids,
        },
    )
    .expect("publish Graph boundary release");
    call_release_activate(env, env.admin, &ReleaseActivateArgs { release_id })
        .expect("activate Graph boundary release");
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
        publish_verified_artifact(&env, CanisterKind::VectorCanister, "0.1.0", vec![b"v0"]),
        publish_verified_artifact(&env, CanisterKind::TextCanister, "0.1.0", vec![b"t0"]),
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
        publish_verified_artifact(&env, CanisterKind::VectorCanister, "0.1.0", vec![b"v0"]),
        publish_verified_artifact(&env, CanisterKind::TextCanister, "0.1.0", vec![b"t0"]),
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
        publish_valid_artifact(&env, CanisterKind::VectorCanister, "1.0.0"),
        publish_valid_artifact(&env, CanisterKind::TextCanister, "1.0.0"),
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

/// `register_graph` provisioned fold (ADR 0056 §6): when a Provision canister is configured, the
/// intent folds into the shared provisioning flow. `deployment_id` derives from the admin
/// principal, so the bootstrap binding must use that value. With no release activated, the
/// admission returns `Reserved` and the fold surfaces success (no created resources to register).
#[test]
fn register_graph_provisioned_fold_admits_with_owner_deployment_id() {
    use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;
    use gleaph_router::types::RegisterGraphArgs;

    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = pic.create_canister();
    pic.add_cycles(router, 2_000_000_000_000);

    let binding = DeploymentBinding {
        deployment_id: admin.to_text(),
        router_principal: router,
        governance_principal: admin,
        binding_version: 1,
        bootstrap_principal: None,
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

    let intent = RegisterGraphArgs {
        graph_name: "provisioned.graph".to_owned(),
        owner: admin,
        admins: std::collections::BTreeSet::new(),
        is_home: false,
        shards: vec![],
        requested_resources: vec![ProvisionableResource {
            logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
        }],
    };

    let bytes = pic
        .update_call(
            router,
            admin,
            "register_graph",
            Encode!(&intent).expect("encode register_graph"),
        )
        .unwrap_or_else(|e| panic!("register_graph on router: {e:?}"));
    let result: Result<(), gleaph_graph_kernel::federation::RouterError> = Decode!(
        &bytes,
        Result<(), gleaph_graph_kernel::federation::RouterError>
    )
    .expect("register_graph provisioned fold must succeed");
    assert!(
        result.is_ok(),
        "register_graph fold must succeed: {result:?}"
    );
}
