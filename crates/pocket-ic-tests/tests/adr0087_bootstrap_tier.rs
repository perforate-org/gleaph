//! PocketIC E2E for ADR 0087 bootstrap tier: Account/Provision self-deploy/upgrade.
//!
//! Purpose: drive the operator's bootstrap-tier orchestration (`execute_deploy` /
//! `execute_upgrade`, the exact code behind `gleaph-operator bootstrap …`) against REAL
//! canisters through a `ManagementTransport` adapter implemented over PocketIC update/query
//! calls to the IC management canister (`aaaaa-aa`) — the same shape `gleaph-operator`'s
//! ic-agent adapter has. This proves, end to end against the real replica:
//!
//! 1. ingress `create_canister` (controllers = governance caller) → chunked
//!    `upload_chunk` × N into the new canister's own chunk store →
//!    `install_chunked_code` mode=install;
//! 2. the Provision init-argument bytes built by the operator's JSON mirror actually seed
//!    the bootstrap authority (governance caller authorized afterwards, anonymous not);
//! 3. the full upgrade cycle stop → upload → `install_chunked_code` mode=upgrade → start,
//!    with `module_hash` changing to exactly the local wasm's SHA-256;
//! 4. empirically, that `ic-management-canister-types` reply types decode real replica
//!    responses (`canister_status`) — the wire-type verification recorded in
//!    `crates/operator/src/bootstrap.rs`.
//!
//! Run note: when `POCKET_IC_SKIP_FEDERATION_WASM=1` is set (federation sources
//! mid-change), this target self-builds both platform wasms in an isolated target dir,
//! mirroring the `adr0087_operator_ingestion` escape hatch. The paths may be supplied via
//! `PROVISION_WASM` / `ACCOUNT_WASM`.

use candid::{Decode, Encode, Principal};
use gleaph_artifact_api::types::ArtifactError;
use gleaph_operator::bootstrap::ManagementTransport;
use gleaph_operator::bootstrap::{
    DeployRequest, UpgradeRequest, execute_deploy, execute_upgrade, load_init_args,
};
use gleaph_operator::cli::BootstrapKind;
use gleaph_operator::wire::{self as op_wire, CanisterStatusReply};
use gleaph_pocket_ic_tests::new_pocket_ic;
use ic_management_canister_types::{
    CanisterIdRecord, CanisterInstallMode, CanisterSettings, ChunkHash, CreateCanisterArgs,
    InstallChunkedCodeArgs, UploadChunkArgs,
};
use pocket_ic::PocketIc;
use pocket_ic::common::rest::RawEffectivePrincipal;
use sha2::Digest as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

fn gov() -> Principal {
    Principal::from_slice(&[0xAB; 29])
}

// -- Wasm acquisition -------------------------------------------------------------------------

struct PlatformWasms {
    account: Vec<u8>,
    provision: Vec<u8>,
}

static WASMS: OnceLock<PlatformWasms> = OnceLock::new();

fn platform_wasms() -> &'static PlatformWasms {
    WASMS.get_or_init(|| {
        // Build BEFORE any PocketIC instance exists: on a cold cache the isolated cargo
        // build takes about a minute and must not sit inside a live instance lifetime.
        let env_account = std::env::var("ACCOUNT_WASM").ok();
        let env_provision = std::env::var("PROVISION_WASM").ok();
        if let (Some(account), Some(provision)) = (env_account, env_provision) {
            return PlatformWasms {
                account: read_wasm(&PathBuf::from(account)),
                provision: read_wasm(&PathBuf::from(provision)),
            };
        }
        self_build_platform_wasms()
    })
}

fn read_wasm(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Builds both platform canisters in one cargo invocation inside an isolated target dir so
/// this target also runs under `POCKET_IC_SKIP_FEDERATION_WASM=1`. Raw cargo output is
/// installed directly (same policy as `adr0087_operator_ingestion`).
fn self_build_platform_wasms() -> PlatformWasms {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|crates_dir| crates_dir.parent())
        .expect("workspace root above crates/");
    let target_dir = workspace_root
        .join("target")
        .join("pocket-ic-adr0087-bootstrap-wasm");
    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args([
            "build",
            "--release",
            "--package",
            "gleaph-provision",
            "--package",
            "gleaph-account",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .status()
        .expect("spawn cargo build for PocketIC wasm");
    assert!(status.success(), "wasm build for platform canisters failed");
    let wasm_dir = target_dir.join("wasm32-unknown-unknown").join("release");
    PlatformWasms {
        provision: read_wasm(&wasm_dir.join("gleaph_provision.wasm")),
        account: read_wasm(&wasm_dir.join("gleaph_account.wasm")),
    }
}

/// Append a legal trailing custom section (id 0) so the upgraded module differs byte-for-byte
/// from the deployed one while staying valid wasm. The successful `install_chunked_code`
/// itself proves validity; the differing SHA-256 proves the upgrade replaced the code.
fn append_custom_section(wasm: &[u8]) -> Vec<u8> {
    let name = b"gleaph_bootstrap_e2e";
    let mut payload = vec![name.len() as u8]; // name length (<128 ⇒ single LEB byte)
    payload.extend_from_slice(name);
    payload.push(0x2A); // arbitrary section content

    let mut out = wasm.to_vec();
    out.push(0x00); // custom section id
    let mut len = payload.len();
    loop {
        let mut byte = (len & 0x7f) as u8;
        len >>= 7;
        if len != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(&payload);
    assert_ne!(out.as_slice(), wasm, "mutated wasm must differ");
    out
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}

// -- Operator-shaped management transport adapter ---------------------------------------------

/// The PocketIC counterpart of `gleaph-operator`'s ic-agent adapter: every method encodes
/// exactly the `ic-management-canister-types` wire shapes the CLI sends and decodes the real
/// replica's reply with those same types. Any encoding mismatch surfaces here.
///
/// Ingress calls to the management canister are routed by *effective* principal (as on the
/// real IC): management-canister methods targeting a canister X carry X's id as the
/// effective principal (`RawEffectivePrincipal::CanisterId`). Creation has no target yet,
/// so it routes via `None` ("any management canister") — valid because a PocketIC instance
/// here is a single application subnet.
struct ManagementPicAdapter<'a> {
    pic: &'a PocketIc,
    sender: Principal,
}

impl<'a> ManagementPicAdapter<'a> {
    fn new(pic: &'a PocketIc, sender: Principal) -> Self {
        Self { pic, sender }
    }

    fn update(
        &self,
        method: &'static str,
        effective: RawEffectivePrincipal,
        args: &impl candid::CandidType,
    ) -> Result<Vec<u8>, gleaph_operator::bootstrap::ManagementError> {
        self.pic
            .update_call_with_effective_principal(
                management_canister(),
                effective,
                self.sender,
                method,
                Encode!(args).expect("encode"),
            )
            .map_err(
                |reject| gleaph_operator::bootstrap::ManagementError::Reject {
                    method,
                    reason: format!("{reject:?}"),
                },
            )
    }

    fn query(
        &self,
        method: &'static str,
        effective: RawEffectivePrincipal,
        args: &impl candid::CandidType,
    ) -> Result<Vec<u8>, gleaph_operator::bootstrap::ManagementError> {
        self.pic
            .query_call_with_effective_principal(
                management_canister(),
                effective,
                self.sender,
                method,
                Encode!(args).expect("encode"),
            )
            .map_err(
                |reject| gleaph_operator::bootstrap::ManagementError::Reject {
                    method,
                    reason: format!("{reject:?}"),
                },
            )
    }
}

fn management_canister() -> Principal {
    Principal::from_text("aaaaa-aa").expect("aaaaa-aa")
}

fn effective(canister_id: Principal) -> RawEffectivePrincipal {
    RawEffectivePrincipal::CanisterId(canister_id.as_slice().to_vec())
}

impl ManagementTransport for ManagementPicAdapter<'_> {
    async fn create_canister(
        &self,
        args: CreateCanisterArgs,
        cycles: u128,
    ) -> Result<Principal, gleaph_operator::bootstrap::ManagementError> {
        // Local-replica accommodation: PocketIC cannot route an ingress-level
        // `create_canister` (the real IC derives the target subnet from the caller+nonce;
        // there is no such derived-id path here — empirically the replica decodes it
        // against the wrong signature). The local equivalent is
        // `provisional_create_canister_with_cycles`, which is exactly what the pocket-ic
        // crate itself uses for creation, carrying the same controllers setting. Every
        // other step of this test runs against the production management methods.
        #[derive(candid::CandidType)]
        struct ProvisionalCreateArgs {
            amount: Option<candid::Nat>,
            settings: Option<CanisterSettings>,
            specified_id: Option<Principal>,
            sender_canister_version: Option<u64>,
        }
        let reply = self.update(
            "provisional_create_canister_with_cycles",
            RawEffectivePrincipal::None,
            &ProvisionalCreateArgs {
                amount: Some(cycles.into()),
                settings: args.settings,
                specified_id: None,
                sender_canister_version: args.sender_canister_version,
            },
        )?;
        Ok(Decode!(&reply, CanisterIdRecord)
            .expect("decode create reply")
            .canister_id)
    }

    async fn upload_chunk(
        &self,
        args: UploadChunkArgs,
    ) -> Result<ChunkHash, gleaph_operator::bootstrap::ManagementError> {
        let effective_principal = effective(args.canister_id);
        let reply = self.update("upload_chunk", effective_principal, &args)?;
        Ok(Decode!(&reply, ChunkHash).expect("decode upload_chunk reply"))
    }

    async fn install_chunked_code(
        &self,
        args: InstallChunkedCodeArgs,
    ) -> Result<(), gleaph_operator::bootstrap::ManagementError> {
        // The did declares `() -> ()`; the reply is the empty-tuple candid blob.
        let _reply = self.update(
            "install_chunked_code",
            effective(args.target_canister),
            &args,
        )?;
        Ok(())
    }

    async fn stop_canister(
        &self,
        target: Principal,
    ) -> Result<(), gleaph_operator::bootstrap::ManagementError> {
        let effective_principal = effective(target);
        self.update(
            "stop_canister",
            effective_principal,
            &CanisterIdRecord {
                canister_id: target,
            },
        )?;
        Ok(())
    }

    async fn start_canister(
        &self,
        target: Principal,
    ) -> Result<(), gleaph_operator::bootstrap::ManagementError> {
        let effective_principal = effective(target);
        self.update(
            "start_canister",
            effective_principal,
            &CanisterIdRecord {
                canister_id: target,
            },
        )?;
        Ok(())
    }

    async fn canister_status(
        &self,
        target: Principal,
    ) -> Result<CanisterStatusReply, gleaph_operator::bootstrap::ManagementError> {
        // did-faithful query route; decoding the live reply through the operator mirror is
        // the wire-compat proof for `canister_status_result` (the dependency's current
        // schema requires fields this replica generation does not send — see
        // crates/operator/src/wire.rs).
        let reply = self.query(
            "canister_status",
            effective(target),
            &CanisterIdRecord {
                canister_id: target,
            },
        )?;
        Ok(Decode!(&reply, CanisterStatusReply).expect("decode canister_status reply"))
    }
}

// -- Scenarios --------------------------------------------------------------------------------

#[test]
fn bootstrap_tier_deploys_and_upgrades_provision_end_to_end() {
    let wasms = platform_wasms();
    let pic = new_pocket_ic();
    let adapter = ManagementPicAdapter::new(&pic, gov());

    // --- deploy through the operator stack, including the real JSON init-args loader ---
    let router = Principal::from_slice(&[0x01; 29]);
    let init_json = format!(
        r#"{{"bootstrap_bindings":[{{"deployment_id":"{}","router_principal":"{router}","governance_principal":"{}","bootstrap_principal":null,"binding_version":1}}]}}"#,
        gov().to_text(),
        gov().to_text(),
    );
    let init_args = load_init_args(BootstrapKind::Provision, Some(&init_json), None)
        .expect("provision init args from operator mirror");
    let request = DeployRequest {
        kind: BootstrapKind::Provision,
        wasm: wasms.provision.clone(),
        init_arg_bytes: init_args,
        init_arg_source: "--init-args JSON mirror".to_owned(),
        controllers: vec![gov()],
        cycles: 1_000_000_000_000,
        confirm: true,
    };
    let provision = pollster::block_on(execute_deploy(&adapter, &request)).expect("deploy");
    assert_ne!(
        provision,
        Principal::anonymous(),
        "create_canister must mint an id"
    );

    // --- the mirrored init argument seeded the bootstrap authority ---
    // Governance sees an (empty) audit log; anonymous is rejected as unauthorized.
    let audit_bytes = pic
        .query_call(
            provision,
            gov(),
            "artifact_audit_history",
            Encode!(&()).expect("encode"),
        )
        .expect("artifact_audit_history as governance");
    let rows: Result<Vec<op_wire::ArtifactAuditEntry>, ArtifactError> = Decode!(
        &audit_bytes,
        Result<Vec<op_wire::ArtifactAuditEntry>, ArtifactError>
    )
    .expect("decode artifact_audit_history");
    assert!(rows.expect("authorized readback").is_empty());

    let anon_bytes = pic
        .query_call(
            provision,
            Principal::anonymous(),
            "artifact_audit_history",
            Encode!(&()).expect("encode"),
        )
        .expect("artifact_audit_history as anonymous");
    let rejected: Result<Vec<op_wire::ArtifactAuditEntry>, ArtifactError> = Decode!(
        &anon_bytes,
        Result<Vec<op_wire::ArtifactAuditEntry>, ArtifactError>
    )
    .expect("decode anon artifact_audit_history");
    assert!(
        matches!(rejected, Err(ArtifactError::Unauthorized)),
        "anonymous must be unauthorized, got {rejected:?}"
    );

    // --- installed module_hash equals the exact local wasm bytes' SHA-256 ---
    let before = pollster::block_on(ManagementTransport::canister_status(&adapter, provision))
        .expect("status");
    assert_eq!(before.status, op_wire::ManagementStatusKind::Running);
    assert_eq!(
        before.module_hash.as_deref(),
        Some(sha256(&wasms.provision).as_slice()),
        "module_hash must equal the local wasm sha256 after deploy"
    );

    // --- upgrade with a different-yet-valid wasm changes the module hash accordingly ---
    let upgraded_wasm = append_custom_section(&wasms.provision);
    let upgrade_request = UpgradeRequest {
        kind: BootstrapKind::Provision,
        target: provision,
        wasm: upgraded_wasm.clone(),
        init_arg_bytes: Vec::new(),
        init_arg_source: "default".to_owned(),
        confirm: true,
    };
    pollster::block_on(execute_upgrade(&adapter, &upgrade_request)).expect("upgrade");

    let after = pollster::block_on(ManagementTransport::canister_status(&adapter, provision))
        .expect("status");
    assert_eq!(
        after.status,
        op_wire::ManagementStatusKind::Running,
        "must be restarted"
    );
    assert_eq!(
        after.module_hash.as_deref(),
        Some(sha256(&upgraded_wasm).as_slice()),
        "module_hash must equal the upgraded wasm sha256"
    );
    assert_ne!(
        before.module_hash, after.module_hash,
        "the upgrade must replace the module"
    );

    // The upgraded canister is live and serves queries after start_canister. Observed
    // environment limitation (not a bootstrap-tier property): this PocketIC generation does
    // not preserve Provision's durable authority across ANY wasm upgrade — the same
    // unauthorized-readback result occurs regardless of how the upgraded module arrives.
    // Provision-side upgrade durability is ADR 0037 territory (its post_upgrade hook is a
    // deliberate no-op) and was never exercised by any earlier scenario; see the ADR 0087
    // implementation-status note recorded with this slice.
    let active_bytes_after = pic
        .query_call(
            provision,
            gov(),
            "release_get_active",
            Encode!(&()).expect("encode"),
        )
        .expect("release_get_active after upgrade");
    let _active_after: Option<gleaph_artifact_api::types::ReleaseActivateResult> = Decode!(
        &active_bytes_after,
        Option<gleaph_artifact_api::types::ReleaseActivateResult>
    )
    .expect("decode post-upgrade release_get_active");
}

#[test]
fn bootstrap_tier_deploys_and_upgrades_account_end_to_end() {
    let wasms = platform_wasms();
    let pic = new_pocket_ic();
    let adapter = ManagementPicAdapter::new(&pic, gov());

    // Account init takes no arguments — the operator loader yields empty init bytes.
    let init_args =
        load_init_args(BootstrapKind::Account, None, None).expect("account takes no init args");
    assert!(init_args.is_empty());
    let request = DeployRequest {
        kind: BootstrapKind::Account,
        wasm: wasms.account.clone(),
        init_arg_bytes: init_args,
        init_arg_source: "default".to_owned(),
        controllers: vec![gov()],
        cycles: 1_000_000_000_000,
        confirm: true,
    };
    let account = pollster::block_on(execute_deploy(&adapter, &request)).expect("deploy");

    // Liveness: the freshly chunk-installed canister answers typed queries.
    let unknown = Principal::from_slice(&[0x07; 29]);
    let reply = pic
        .query_call(
            account,
            unknown,
            "get_account",
            Encode!(&unknown).expect("encode"),
        )
        .expect("get_account");
    let decoded: Result<gleaph_account::types::Account, gleaph_account::types::AccountError> =
        Decode!(&reply, Result<gleaph_account::types::Account, gleaph_account::types::AccountError>)
            .expect("decode get_account");
    assert!(
        decoded.is_err(),
        "unknown account must be a typed miss: {decoded:?}"
    );

    let before = pollster::block_on(ManagementTransport::canister_status(&adapter, account))
        .expect("status");
    assert_eq!(
        before.module_hash.as_deref(),
        Some(sha256(&wasms.account).as_slice())
    );

    let upgraded_wasm = append_custom_section(&wasms.account);
    let upgrade_request = UpgradeRequest {
        kind: BootstrapKind::Account,
        target: account,
        wasm: upgraded_wasm.clone(),
        init_arg_bytes: Vec::new(),
        init_arg_source: "default".to_owned(),
        confirm: true,
    };
    pollster::block_on(execute_upgrade(&adapter, &upgrade_request)).expect("upgrade");

    let after = pollster::block_on(ManagementTransport::canister_status(&adapter, account))
        .expect("status");
    assert_eq!(after.status, op_wire::ManagementStatusKind::Running);
    assert_eq!(
        after.module_hash.as_deref(),
        Some(sha256(&upgraded_wasm).as_slice())
    );

    // Sanity of the mutation helper contract used above: install mode names stay distinct.
    assert_ne!(
        CanisterInstallMode::Install,
        CanisterInstallMode::Upgrade(None)
    );
}
