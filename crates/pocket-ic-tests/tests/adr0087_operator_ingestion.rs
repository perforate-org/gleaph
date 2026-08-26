//! PocketIC E2E for ADR 0087 slice 3: the operator transport stack.
//!
//! Purpose: prove that the `gleaph-artifact-api` wire-mirror types are candid-compatible with
//! the REAL Provision canister by driving the shared idempotent ingest driver through an
//! `ArtifactTransport` adapter implemented over PocketIC update/query calls — the same shape
//! `gleaph-operator`'s ic-agent adapter has. Also proves runtime compatibility of the
//! operator-only mirrors (`release_install`, `admin_install_deployment_binding`,
//! `artifact_audit_history`) and, statically, both encode directions against the server's own
//! Rust types.
//!
//! Run note: when `POCKET_IC_SKIP_FEDERATION_WASM=1` is set (federation sources mid-change),
//! this target self-builds the provision wasm in an isolated target dir, mirroring the
//! `text_index_provisioning` escape hatch. The wasm path may be supplied via `PROVISION_WASM`.

use candid::{Decode, Encode, Principal};
use gleaph_artifact_api::ArtifactTransport;
use gleaph_artifact_api::driver::{IngestError, IngestOutcome, IngestOutcome::*, ingest_artifact};
use gleaph_artifact_api::pipeline::plan_artifact;
use gleaph_artifact_api::types::{
    ArtifactError, ArtifactId, ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUpload,
    CanisterKind, MAX_CHUNK_BYTES, ReleaseActivateArgs, ReleaseActivateResult, ReleaseError,
    ReleaseManifest, ReleasePublishArgs,
};
use gleaph_operator::wire as op_wire;
use gleaph_pocket_ic_tests::new_pocket_ic;
use pocket_ic::PocketIc;
use std::future::Future;
use std::path::PathBuf;
use std::process::Command;

const VERSION: &str = "0.8.7";
const RELEASE_ID: &str = "release-adr0087-operator";

// -- Wasm acquisition -------------------------------------------------------------------------

/// Reads a wasm artifact from `env_var` when set (the build.rs-managed fast path), otherwise
/// builds `gleaph-provision` in an isolated target dir so this target also runs under
/// `POCKET_IC_SKIP_FEDERATION_WASM=1`. Raw cargo output is installed directly.
fn ensure_provision_wasm(env_var: &str) -> Vec<u8> {
    if let Ok(path) = std::env::var(env_var) {
        return std::fs::read(&path).unwrap_or_else(|e| panic!("read {env_var} {}: {e}", path));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|crates_dir| crates_dir.parent())
        .expect("workspace root above crates/");
    let target_dir = workspace_root.join("target").join("pocket-ic-adr0087-wasm");
    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args([
            "build",
            "--release",
            "--package",
            "gleaph-provision",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .status()
        .expect("spawn cargo build for PocketIC wasm");
    assert!(status.success(), "wasm build for gleaph-provision failed");
    let wasm_path = target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join("gleaph_provision.wasm");
    std::fs::read(&wasm_path).unwrap_or_else(|e| panic!("read {}: {e}", wasm_path.display()))
}

// -- Bootstrap --------------------------------------------------------------------------------

struct Env {
    pic: PocketIc,
    admin: Principal,
    provision: Principal,
}

fn bootstrap(provision_wasm: Vec<u8>) -> Env {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    // `deployment_id` derives from the governance principal (ADR 0068 convention).
    let binding = gleaph_provision::types::DeploymentBinding {
        deployment_id: admin.to_text(),
        router_principal: Principal::from_slice(&[0x01; 29]),
        governance_principal: admin,
        binding_version: 1,
        bootstrap_principal: None,
    };
    let provision = pic.create_canister();
    pic.add_cycles(provision, 100_000_000_000_000);
    pic.install_canister(
        provision,
        provision_wasm,
        Encode!(&gleaph_provision::canister::init::ProvisionInitArgs {
            bootstrap_bindings: vec![binding],
        })
        .expect("encode provision init"),
        None,
    );

    Env {
        pic,
        admin,
        provision,
    }
}

// -- Operator-shaped transport adapter --------------------------------------------------------

/// The PocketIC counterpart of `gleaph-operator`'s ic-agent adapter: implements the slice-2
/// `ArtifactTransport` trait with one candid call per method, carrying exactly the mirror
/// types. Any wire mismatch surfaces here as a decode failure against the real canister.
struct OperatorPicAdapter<'a> {
    pic: &'a PocketIc,
    provision: Principal,
    sender: Principal,
}

impl<'a> OperatorPicAdapter<'a> {
    fn new(env: &'a Env, sender: Principal) -> Self {
        Self {
            pic: &env.pic,
            provision: env.provision,
            sender,
        }
    }

    fn update<T: candid::CandidType + serde::de::DeserializeOwned>(
        &self,
        method: &str,
        args: &impl candid::CandidType,
    ) -> T {
        let bytes = self
            .pic
            .update_call(
                self.provision,
                self.sender,
                method,
                Encode!(args).expect("encode update args"),
            )
            .unwrap_or_else(|e| panic!("{method} rejected by the replica: {e:?}"));
        Decode!(&bytes, T).unwrap_or_else(|e| panic!("decode {method} reply: {e}"))
    }
}

impl ArtifactTransport for OperatorPicAdapter<'_> {
    fn publish_metadata(
        &self,
        args: ArtifactPublishMetadataArgs,
    ) -> impl Future<Output = Result<ArtifactMetadata, ArtifactError>> + Send {
        async move {
            self.update::<Result<ArtifactMetadata, ArtifactError>>(
                "artifact_publish_metadata",
                &args,
            )
        }
    }

    fn upload_chunk(
        &self,
        args: gleaph_artifact_api::types::ArtifactUploadChunkArgs,
    ) -> impl Future<Output = Result<ArtifactUpload, ArtifactError>> + Send {
        async move {
            self.update::<Result<ArtifactUpload, ArtifactError>>("artifact_upload_chunk", &args)
        }
    }

    fn get_status(
        &self,
        artifact_id: ArtifactId,
    ) -> impl Future<Output = Result<Option<ArtifactUpload>, ArtifactError>> + Send {
        async move {
            // did: `artifact_get_status : (ArtifactId) -> (opt ArtifactUpload) query` — a
            // plain option with no typed Err channel; the transport maps it to Ok.
            let bytes = self
                .pic
                .query_call(
                    self.provision,
                    self.sender,
                    "artifact_get_status",
                    Encode!(&artifact_id).expect("encode artifact_get_status"),
                )
                .expect("artifact_get_status query");
            Ok(Decode!(&bytes, Option<ArtifactUpload>).expect("decode artifact_get_status"))
        }
    }

    fn release_publish(
        &self,
        args: ReleasePublishArgs,
    ) -> impl Future<Output = Result<ReleaseManifest, ReleaseError>> + Send {
        async move { self.update::<Result<ReleaseManifest, ReleaseError>>("release_publish", &args) }
    }

    fn release_activate(
        &self,
        args: ReleaseActivateArgs,
    ) -> impl Future<Output = Result<ReleaseActivateResult, ReleaseError>> + Send {
        async move {
            self.update::<Result<ReleaseActivateResult, ReleaseError>>("release_activate", &args)
        }
    }
}

// -- Helpers ----------------------------------------------------------------------------------

/// Deterministic multi-chunk payload (3 chunks).
fn payload() -> Vec<u8> {
    (0..MAX_CHUNK_BYTES * 2 + 5)
        .map(|i| (i % 251) as u8)
        .collect()
}

fn expect_verified(outcome: Result<IngestOutcome, IngestError>) -> IngestOutcome {
    match outcome.expect("ingest must not fail") {
        verified @ Verified { .. } => verified,
        AwaitingVerification { artifact_id } => {
            panic!("driver reported AwaitingVerification for {artifact_id:?}")
        }
    }
}

// -- Scenarios --------------------------------------------------------------------------------

#[test]
fn operator_transport_drives_real_provision_ingestion() {
    // Acquire the wasm BEFORE the PocketIC instance exists: on a cold cache the isolated
    // cargo build takes about a minute, and that idle window must not sit inside a live
    // instance's connection lifetime.
    let provision_wasm = ensure_provision_wasm("PROVISION_WASM");
    let env = bootstrap(provision_wasm);
    let adapter = OperatorPicAdapter::new(&env, env.admin);

    // --- (1) multi-chunk ingest through the shared driver reaches durable verification ---
    let bytes = payload();
    let plan = plan_artifact(&bytes, CanisterKind::Graph, VERSION).expect("plan");
    assert_eq!(
        plan.chunk_count(),
        3,
        "payload must split into three chunks"
    );

    let first = expect_verified(pollster::block_on(ingest_artifact(&plan, &adapter)));
    match first {
        Verified {
            verified_at_ns: Some(_),
        } => {}
        other => panic!("first ingest must carry the verification timestamp: {other:?}"),
    }

    // --- (2) idempotent re-run: status None → equal ConflictingMetadata → conflict-signal
    //         resolution to Verified (the ambiguous-resume contract from ADR 0087 slice 2) ---
    let second = pollster::block_on(ingest_artifact(&plan, &adapter));
    assert!(
        matches!(second, Ok(Verified { .. })),
        "re-ingest must resolve idempotently to Verified, got {second:?}"
    );

    // --- (3) get_status after verify decodes the plain-opt wire as None (row reclaimed) ---
    let status = pollster::block_on(ArtifactTransport::get_status(
        &adapter,
        plan.artifact_id.clone(),
    ));
    assert_eq!(status.expect("get_status"), None);

    // --- (4) anonymous caller is rejected with the typed mirror error ---
    let anon = OperatorPicAdapter::new(&env, Principal::anonymous());
    let rejection = pollster::block_on(anon.publish_metadata(plan.publish_args.clone()));
    assert!(
        matches!(rejection, Err(ArtifactError::Unauthorized)),
        "anonymous publish must decode to Unauthorized, got {rejection:?}"
    );

    // --- (5) publish the remaining four kinds so a complete release exists ---
    let mut ids = vec![plan.artifact_id.clone()];
    for kind in [
        CanisterKind::Router,
        CanisterKind::PropertyIndex,
        CanisterKind::VectorCanister,
        CanisterKind::TextCanister,
    ] {
        let tiny = plan_artifact(b"operator-e2e-tiny-wasm", kind, VERSION).expect("tiny plan");
        let outcome = pollster::block_on(ingest_artifact(&tiny, &adapter));
        assert!(
            matches!(outcome, Ok(Verified { .. })),
            "{kind:?}: {outcome:?}"
        );
        ids.push(tiny.artifact_id);
    }

    // --- (6) release_publish returns the canonicalized manifest over mirrored types ---
    let manifest = pollster::block_on(adapter.release_publish(ReleasePublishArgs {
        release_id: gleaph_artifact_api::types::ReleaseId(RELEASE_ID.to_owned()),
        artifact_ids: ids.clone(),
    }))
    .expect("release_publish ok");
    assert_eq!(manifest.release_id.0, RELEASE_ID);
    assert_eq!(manifest.graph_artifact, ids[0]);
    assert_eq!(manifest.router_artifact, ids[1]);
    assert_eq!(manifest.property_index_artifact, ids[2]);
    assert_eq!(manifest.vector_canister_artifact, ids[3]);
    assert_eq!(manifest.text_canister_artifact, ids[4]);

    // --- (7) release_activate swaps atomically; release_get_active reads it back ---
    let activated = pollster::block_on(adapter.release_activate(ReleaseActivateArgs {
        release_id: gleaph_artifact_api::types::ReleaseId(RELEASE_ID.to_owned()),
    }))
    .expect("release_activate ok");
    assert_eq!(activated.release_id.0, RELEASE_ID);
    assert_eq!(activated.previous_release_id, None);

    let active_bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "release_get_active",
            Encode!(&()).expect("encode release_get_active"),
        )
        .expect("release_get_active query");
    let active: Option<ReleaseActivateResult> =
        Decode!(&active_bytes, Option<ReleaseActivateResult>).expect("decode opt");
    let active = active.expect("an active release must exist");
    assert_eq!(active.release_id.0, RELEASE_ID);

    // --- (8) binding install through the OPERATOR mirror types (runtime compat proof) ---
    let router = Principal::from_slice(&[0x02; 29]);
    let entry_bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_install_deployment_binding",
            Encode!(&op_wire::AdminInstallDeploymentBindingArgs {
                binding_version: 2,
                router_principal: router,
                governance_principal: env.admin,
                bootstrap_principal: None,
                deployment_id: "operator-e2e-deployment".to_owned(),
            })
            .expect("encode binding install"),
        )
        .expect("admin_install_deployment_binding");
    let entry: Result<op_wire::BootstrapAuthEntry, op_wire::AdminInstallError> =
        Decode!(&entry_bytes, Result<op_wire::BootstrapAuthEntry, op_wire::AdminInstallError>)
            .expect("decode binding install");
    let entry = entry.expect("binding install ok");
    assert_eq!(entry.action, op_wire::BootstrapAuthAction::AdminInstall);
    assert_eq!(
        entry.deployment_id.as_deref(),
        Some("operator-e2e-deployment")
    );

    // Non-authority installing a NEW deployment decodes to UnknownDeployment.
    let stranger = Principal::from_slice(&[0x03; 29]);
    let reject_bytes = env
        .pic
        .update_call(
            env.provision,
            stranger,
            "admin_install_deployment_binding",
            Encode!(&op_wire::AdminInstallDeploymentBindingArgs {
                binding_version: 1,
                router_principal: router,
                governance_principal: stranger,
                bootstrap_principal: None,
                deployment_id: "stranger-deployment".to_owned(),
            })
            .expect("encode stranger binding install"),
        )
        .expect("stranger admin_install_deployment_binding");
    let rejected: Result<op_wire::BootstrapAuthEntry, op_wire::AdminInstallError> =
        Decode!(&reject_bytes, Result<op_wire::BootstrapAuthEntry, op_wire::AdminInstallError>)
            .expect("decode stranger binding install");
    assert!(
        matches!(
            rejected,
            Err(op_wire::AdminInstallError::UnknownDeployment(_))
        ),
        "stranger must be UnknownDeployment-rejected, got {rejected:?}"
    );

    // --- (9) audit history readback through the OPERATOR mirror types ---
    let audit_bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "artifact_audit_history",
            Encode!(&()).expect("encode artifact_audit_history"),
        )
        .expect("artifact_audit_history query");
    let rows: Result<Vec<op_wire::ArtifactAuditEntry>, ArtifactError> = Decode!(
        &audit_bytes,
        Result<Vec<op_wire::ArtifactAuditEntry>, ArtifactError>
    )
    .expect("decode artifact_audit_history");
    let rows = rows.expect("audit history ok");
    assert!(
        !rows.is_empty(),
        "audit log must have recorded the operations"
    );
    let actions: Vec<op_wire::ArtifactAuditAction> = rows.iter().map(|row| row.action).collect();
    assert!(actions.contains(&op_wire::ArtifactAuditAction::UploadChunk));
    assert!(actions.contains(&op_wire::ArtifactAuditAction::VerifyArtifact));
    assert!(actions.contains(&op_wire::ArtifactAuditAction::PublishRelease));
    assert!(actions.contains(&op_wire::ArtifactAuditAction::ActivateRelease));
}

/// Static wire-compatibility proof for every operator-only mirror type: encoding with the
/// operator mirror must decode with the server's own type and vice versa, field-for-field.
#[test]
fn operator_mirrors_round_trip_through_server_types() {
    use gleaph_operator::wire as op;
    use gleaph_provision::types as server;

    let kind = server::CanisterKind::TextCanister;
    let sha: [u8; 32] = [9; 32];
    let artifact = server::ArtifactId::new(kind.clone(), VERSION.to_owned(), sha);

    // ReleaseInstallArgs (both directions).
    let op_args = op::ReleaseInstallArgs {
        target_canister_kind: CanisterKind::TextCanister,
        registry_version: 7,
        install_args: vec![1, 2, 3],
        target_canister_id: Some(Principal::from_slice(&[0x04; 29])),
    };
    let decoded: server::ReleaseInstallArgs = Decode!(
        &Encode!(&op_args).expect("encode"),
        server::ReleaseInstallArgs
    )
    .expect("decode");
    assert_eq!(decoded.target_canister_kind, kind);
    assert_eq!(decoded.registry_version, 7);
    assert_eq!(decoded.install_args, vec![1, 2, 3]);
    let back: op::ReleaseInstallArgs = Decode!(
        &Encode!(&server::ReleaseInstallArgs {
            target_canister_kind: kind.clone(),
            target_canister_id: decoded.target_canister_id,
            install_args: decoded.install_args.clone(),
            registry_version: 7,
        })
        .expect("encode server"),
        op::ReleaseInstallArgs
    )
    .expect("decode server→operator");
    assert_eq!(back, op_args);

    // ReleaseInstallResult (server → operator).
    let server_result = server::ReleaseInstallResult {
        release_id: server::ReleaseId(RELEASE_ID.to_owned()),
        target_canister_id: Principal::from_slice(&[0x04; 29]),
        installed_chunks: 3,
        install_chunked_code_hash: sha,
        installed_at_ns: 42,
    };
    let op_result: op::ReleaseInstallResult = Decode!(
        &Encode!(&server_result).expect("encode server result"),
        op::ReleaseInstallResult
    )
    .expect("decode server result");
    assert_eq!(op_result.release_id.0, RELEASE_ID);
    assert_eq!(op_result.installed_chunks, 3);
    assert_eq!(op_result.install_chunked_code_hash, sha);

    // InstallError variants (both directions, one representative per variant).
    let install_errors = [
        op::InstallError::NoActiveRelease,
        op::InstallError::ArtifactNotFound(op_args_target_artifact()),
        op::InstallError::ArtifactNotVerified(op_args_target_artifact()),
        op::InstallError::TargetCanisterKindForbidden(CanisterKind::VectorCanister),
        op::InstallError::ManagementCanisterCallFailed("boom".to_owned()),
        op::InstallError::ChunkStoreNotReconciled,
        op::InstallError::Unauthorized,
        op::InstallError::NoBootstrapAuthority,
    ];
    for variant in install_errors {
        let bytes = Encode!(&variant).expect("encode install error");
        // Operator → server.
        let server_variant: server::InstallError = Decode!(&bytes, server::InstallError)
            .unwrap_or_else(|e| panic!("server cannot decode operator InstallError: {e}"));
        // Server → operator, preserving identity across the round trip.
        let bytes_back = Encode!(&server_variant).expect("re-encode decoded server InstallError");
        let round: op::InstallError = Decode!(&bytes_back, op::InstallError)
            .unwrap_or_else(|e| panic!("operator cannot decode server InstallError: {e}"));
        assert_eq!(
            format!("{round:?}"),
            format!("{variant:?}"),
            "variant identity"
        );
    }

    // AdminInstallDeploymentBindingArgs + AdminInstallError (both directions).
    let op_binding = op::AdminInstallDeploymentBindingArgs {
        binding_version: 3,
        router_principal: Principal::from_slice(&[0x05; 29]),
        governance_principal: Principal::from_slice(&[0x06; 29]),
        bootstrap_principal: Some(Principal::from_slice(&[0x07; 29])),
        deployment_id: "d".to_owned(),
    };
    let _: server::AdminInstallDeploymentBindingArgs = Decode!(
        &Encode!(&op_binding).expect("encode"),
        server::AdminInstallDeploymentBindingArgs
    )
    .expect("server decodes operator binding args");
    let back: op::AdminInstallDeploymentBindingArgs = Decode!(
        &Encode!(&server::AdminInstallDeploymentBindingArgs {
            deployment_id: "d".to_owned(),
            router_principal: Principal::from_slice(&[0x05; 29]),
            governance_principal: Principal::from_slice(&[0x06; 29]),
            binding_version: 3,
            bootstrap_principal: Some(Principal::from_slice(&[0x07; 29])),
        })
        .expect("encode server binding args"),
        op::AdminInstallDeploymentBindingArgs
    )
    .expect("operator decodes server binding args");
    assert_eq!(back, op_binding);

    let admin_errors = [
        op::AdminInstallError::UnknownDeployment("u".to_owned()),
        op::AdminInstallError::AlreadyExists {
            existing_governance: Principal::from_slice(&[0x06; 29]),
            deployment_id: "d".to_owned(),
        },
        op::AdminInstallError::InvalidState("s".to_owned()),
    ];
    for variant in admin_errors {
        let bytes = Encode!(&variant).expect("encode admin error");
        let server_variant: server::AdminInstallError = Decode!(&bytes, server::AdminInstallError)
            .unwrap_or_else(|e| panic!("server cannot decode operator AdminInstallError: {e}"));
        let bytes_back =
            Encode!(&server_variant).expect("re-encode decoded server AdminInstallError");
        let round: op::AdminInstallError = Decode!(&bytes_back, op::AdminInstallError)
            .unwrap_or_else(|e| panic!("operator cannot decode server AdminInstallError: {e}"));
        assert_eq!(
            format!("{round:?}"),
            format!("{variant:?}"),
            "variant identity"
        );
    }

    // BootstrapAuthEntry (server → operator).
    let server_entry = server::BootstrapAuthEntry {
        caller: Principal::from_slice(&[0xAB; 29]),
        deployment_id: Some("d".to_owned()),
        action: server::BootstrapAuthAction::AdminInstall,
        timestamp_ns: 5,
        registry_version: Some(2),
    };
    let op_entry: op::BootstrapAuthEntry = Decode!(
        &Encode!(&server_entry).expect("encode server entry"),
        op::BootstrapAuthEntry
    )
    .expect("decode server entry");
    assert_eq!(op_entry.action, op::BootstrapAuthAction::AdminInstall);
    assert_eq!(op_entry.registry_version, Some(2));

    // ArtifactAuditEntry incl. action + outcome enums (both directions).
    let op_audit = op::ArtifactAuditEntry {
        action: op::ArtifactAuditAction::InstallRelease,
        timestamp_ns: 11,
        artifact_id: Some(artifact_to_op(&artifact)),
        release_id: Some(gleaph_artifact_api::types::ReleaseId(RELEASE_ID.to_owned())),
        target_canister: Some(Principal::from_slice(&[0x04; 29])),
        caller: Principal::from_slice(&[0xAB; 29]),
        outcome: op::ArtifactAuditOutcome::Success,
        deployment_id: Some("d".to_owned()),
        reason: None,
    };
    let _: server::ArtifactAuditEntry = Decode!(
        &Encode!(&op_audit).expect("encode operator audit"),
        server::ArtifactAuditEntry
    )
    .expect("server decodes operator audit entry");
    let server_audit = server::ArtifactAuditEntry {
        action: server::ArtifactAuditAction::VerifyArtifact,
        timestamp_ns: 12,
        artifact_id: Some(artifact),
        release_id: Some(server::ReleaseId(RELEASE_ID.to_owned())),
        target_canister: None,
        caller: Principal::from_slice(&[0xAB; 29]),
        outcome: server::ArtifactAuditOutcome::Rejected,
        deployment_id: None,
        reason: Some("why".to_owned()),
    };
    let op_decoded: op::ArtifactAuditEntry = Decode!(
        &Encode!(&server_audit).expect("encode server audit"),
        op::ArtifactAuditEntry
    )
    .expect("operator decodes server audit entry");
    assert_eq!(op_decoded.action, op::ArtifactAuditAction::VerifyArtifact);
    assert_eq!(op_decoded.outcome, op::ArtifactAuditOutcome::Rejected);
    assert_eq!(op_decoded.reason.as_deref(), Some("why"));
}

fn op_args_target_artifact() -> ArtifactId {
    ArtifactId::new(CanisterKind::VectorCanister, VERSION.to_owned(), [9; 32])
}

/// Convert a server-side artifact id into the operator mirror, field by field (the structs
/// declare fields in different orders; candid matches by name).
fn artifact_to_op(id: &gleaph_provision::types::ArtifactId) -> ArtifactId {
    let kind = match id.canister_kind {
        gleaph_provision::types::CanisterKind::Router => CanisterKind::Router,
        gleaph_provision::types::CanisterKind::Graph => CanisterKind::Graph,
        gleaph_provision::types::CanisterKind::PropertyIndex => CanisterKind::PropertyIndex,
        gleaph_provision::types::CanisterKind::VectorCanister => CanisterKind::VectorCanister,
        gleaph_provision::types::CanisterKind::TextCanister => CanisterKind::TextCanister,
    };
    ArtifactId::new(kind, id.semantic_version.clone(), id.sha256)
}
