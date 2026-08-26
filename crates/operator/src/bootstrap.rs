//! Bootstrap tier: Account/Provision self-deploy/upgrade through the IC management canister
//! (ADR 0087 §Explicitly deferred — delivered before first production operation).
//!
//! ADR 0036 excludes Provision's own wasm from the artifact catalog: governance installs or
//! upgrades Account/Provision through a separate bootstrap procedure. This module is that
//! procedure as tooling. Every canister-facing call goes through the slice-3 [`IcIngress`]
//! layer (`crate::transport`) pointed at the management canister (`aaaaa-aa`) — no second
//! transport stack exists.
//!
//! # Wire types (SSOT decision)
//!
//! All management-canister shapes come from `ic-management-canister-types` (workspace
//! dependency, also used by the dev CLI) — no hand mirrors. Verified field-for-field against
//! the official management did (docs.internetcomputer.org/references/ic.did):
//! `upload_chunk_args`, `install_chunked_code_args` (including the
//! `upgrade : opt upgrade_flags` payload), `create_canister_args/result`,
//! `start/stop/canister_status_args`, and the `canister_status_result` /
//! `definite_canister_settings` / `memory_metrics` blocks all match by field name, which is
//! what candid wire compatibility rests on. The single source-level divergence found — the
//! docs page omits `memory_metrics.log_memory_store_size`, which the crate (mirroring the
//! dfinity/ic monorepo did) requires — was resolved empirically: the PocketIC E2E decodes a
//! live replica reply with the dependency type, proving this replica generation sends it.
//!
//! # PocketIC environment notes (recorded E2E findings)
//!
//! - The pinned PocketIC replica answers `canister_status` with the older reply schema
//!   (no `status_visibility`/`minimum_incoming_canister_call_cycles` in settings, no
//!   `log_memory_store_size` in memory metrics), which the current
//!   `ic-management-canister-types` requires — hence the hand-mirrored reply above. Verified
//!   by decoding one live reply both ways during development.
//! - Ingress `create_canister` has no derived-effective-id routing in PocketIC; its local
//!   equivalent is `provisional_create_canister_with_cycles` (what the pocket-ic crate
//!   itself uses), carrying the same controllers setting. The ic-agent
//!   [`ManagementClient`] selects that method for every non-mainnet connection
//!   (GAP-2026-08-24-006(a)); the PocketIC E2E adapter mirrors the same choice.
//!
//! # Safety model
//!
//! - Plan/confirm: without `--yes` every command prints the exact execution plan and stops.
//! - Upgrade shows the current `module_hash` before touching anything and prints the new
//!   `module_hash` next to the local wasm SHA-256 afterwards (verification material).
//! - Failure after `stop_canister` never auto-starts: the target is left stopped and the
//!   failure is reported with resume instructions (re-running is safe; chunk uploads are
//!   idempotent and `install_chunked_code` replaces code wholesale).
//!
//! # Known transport gap
//!
//! `create_canister` on mainnet requires attaching at least the creation fee in cycles, and
//! ic-agent 0.49.2 cannot express attached cycles on ingress requests (its update builder has
//! no cycles field). Deploys targeting the `ic` network selector therefore fail fast with an
//! explicit explanation instead of sending a call the replica must reject. Fee-free replicas
//! (PocketIC/local/custom test endpoints) are unaffected; the [`ManagementTransport`] seam
//! carries `cycles` so only the transport impl changes when the agent gains support.

#![allow(clippy::manual_async_fn)]

use std::future::Future;

use candid::{CandidType, Decode, Encode, Principal};
use ic_management_canister_types::{
    CanisterIdRecord, CanisterInstallMode, CanisterSettings, ChunkHash, CreateCanisterArgs,
    InstallChunkedCodeArgs, ProvisionalCreateCanisterWithCyclesArgs, UploadChunkArgs,
};
use serde::Deserialize;
use thiserror::Error;

use crate::cli::BootstrapKind;
use crate::encoding::to_hex;
use crate::error::OperatorError;
use crate::transport::IcIngress;
use crate::wire::{CanisterStatusReply, ManagementStatusKind};

/// Cycles attached to `create_canister` when `--cycles` is omitted. Matches the amount
/// Provision's issuance attaches to freshly created canisters (INITIAL_CANISTER_CYCLES,
/// `crates/provision/src/canister/mod.rs`). The amount is intentionally bounded to first
/// memory growth and install; durable cycle policy remains ADR 0038 territory.
const DEFAULT_CREATE_CYCLES: u128 = 1_000_000_000_000;

/// The management-canister principal (`aaaaa-aa`) — destination of every call this module
/// makes through the shared ingress layer.
fn management_canister() -> Principal {
    Principal::from_text("aaaaa-aa").expect("aaaaa-aa is a valid principal")
}

/// Failures of a management-canister call. The management canister has no typed error
/// channel — every rejection arrives as replica reject text.
#[derive(Debug, Error)]
pub enum ManagementError {
    /// The replica rejected the call (authorization, bounds, state).
    #[error("management canister rejected {method}: {reason}")]
    Reject {
        /// Method that was rejected.
        method: &'static str,
        /// Replica reject message.
        reason: String,
    },
    /// The call never reached a reply (connectivity, agent failure).
    #[error("management canister call {method} failed: {reason}")]
    Transport {
        /// Method whose call failed.
        method: &'static str,
        /// Underlying failure text.
        reason: String,
    },
}

impl From<ManagementError> for OperatorError {
    fn from(error: ManagementError) -> Self {
        Self::Message(error.to_string())
    }
}

/// Transport seam for the bootstrap tier: one management-canister caller.
///
/// The ic-agent implementation lives in [`ManagementClient`]; the PocketIC E2E provides its
/// own implementation over update/query calls so the exact same orchestration and wire types
/// run against the real replica.
pub trait ManagementTransport {
    /// Create a canister with `args.settings.controllers`, attaching `cycles`.
    fn create_canister(
        &self,
        args: CreateCanisterArgs,
        cycles: u128,
    ) -> impl Future<Output = Result<Principal, ManagementError>> + Send;

    /// Upload one ≤1 MiB chunk into `args.canister_id`'s chunk store; returns its hash.
    fn upload_chunk(
        &self,
        args: UploadChunkArgs,
    ) -> impl Future<Output = Result<ic_management_canister_types::ChunkHash, ManagementError>> + Send;

    /// Assemble stored chunks and install/upgrade the target's code.
    fn install_chunked_code(
        &self,
        args: InstallChunkedCodeArgs,
    ) -> impl Future<Output = Result<(), ManagementError>> + Send;

    /// Stop the canister (required before upgrade).
    fn stop_canister(
        &self,
        target: Principal,
    ) -> impl Future<Output = Result<(), ManagementError>> + Send;

    /// Start a previously stopped canister.
    fn start_canister(
        &self,
        target: Principal,
    ) -> impl Future<Output = Result<(), ManagementError>> + Send;

    /// Read canister status (caller must be a controller).
    ///
    /// The reply is the hand-mirrored [`CanisterStatusReply`] — see `crate::wire` for the
    /// schema-generation rationale.
    fn canister_status(
        &self,
        target: Principal,
    ) -> impl Future<Output = Result<CanisterStatusReply, ManagementError>> + Send;
}

/// ic-agent-backed management surface over the shared [`IcIngress`] layer.
pub struct ManagementClient<'a> {
    ingress: &'a IcIngress,
}

impl<'a> ManagementClient<'a> {
    /// Build a client issuing ingress calls to the management canister.
    pub fn new(ingress: &'a IcIngress) -> Self {
        Self { ingress }
    }

    async fn unit_call(
        &self,
        method: &'static str,
        encoded: Vec<u8>,
        effective_canister_id: Principal,
    ) -> Result<(), ManagementError> {
        self.ingress
            .update_raw_with_effective_canister_id(
                management_canister(),
                method,
                encoded,
                effective_canister_id,
            )
            .await
            .map(|_| ())
            .map_err(|error| transport_or_reject(method, error.to_string()))
    }
}

/// Classify an ingress failure text: ic-agent surfaces replica rejects as certified or
/// uncertified rejects carrying the reject message; everything else is transport.
fn transport_or_reject(method: &'static str, text: String) -> ManagementError {
    if text.contains("reject") {
        ManagementError::Reject {
            method,
            reason: text,
        }
    } else {
        ManagementError::Transport {
            method,
            reason: text,
        }
    }
}

impl ManagementTransport for ManagementClient<'_> {
    async fn create_canister(
        &self,
        args: CreateCanisterArgs,
        cycles: u128,
    ) -> Result<Principal, ManagementError> {
        if self.ingress.is_mainnet() {
            // ic-agent 0.49.2 has no way to attach cycles to an ingress request (see module
            // docs); deploys on the mainnet selector are refused earlier for exactly this
            // reason. Fee-free replicas accept the call without attached cycles.
            let _ = cycles;
            let record: CanisterIdRecord = self
                .ingress
                .update_value(management_canister(), "create_canister", &args)
                .await
                .map_err(|error| transport_or_reject("create_canister", error.to_string()))?;
            Ok(record.canister_id)
        } else {
            // Local/PocketIC endpoints cannot route an ingress-level `create_canister` (the
            // real IC derives the target subnet from the caller+nonce; there is no such
            // derived-id path here — the replica decodes it against the wrong signature).
            // The local equivalent is `provisional_create_canister_with_cycles`, which is
            // exactly what the pocket-ic crate itself uses for creation, carrying the same
            // controllers setting. Its response certification requires the effective canister
            // id to fall within the target subnet's canister ranges, so we use the network's
            // default effective canister id from `/_/topology` (GAP-2026-08-24-006(a)).
            let effective = self
                .ingress
                .default_effective_canister_id()
                .ok_or_else(|| ManagementError::Transport {
                    method: "provisional_create_canister_with_cycles",
                    reason: "local network did not expose a default effective canister id \
                                 (/_/topology); cannot route the provisional create"
                        .to_owned(),
                })?;
            let reply = self
                .ingress
                .update_raw_with_effective_canister_id(
                    management_canister(),
                    "provisional_create_canister_with_cycles",
                    Encode!(&ProvisionalCreateCanisterWithCyclesArgs {
                        amount: Some(cycles.into()),
                        settings: args.settings,
                        specified_id: None,
                        sender_canister_version: args.sender_canister_version,
                    })
                    .expect("encode provisional create args"),
                    effective,
                )
                .await
                .map_err(|error| {
                    transport_or_reject(
                        "provisional_create_canister_with_cycles",
                        error.to_string(),
                    )
                })?;
            let record =
                Decode!(&reply, CanisterIdRecord).expect("decode provisional create reply");
            Ok(record.canister_id)
        }
    }

    async fn upload_chunk(&self, args: UploadChunkArgs) -> Result<ChunkHash, ManagementError> {
        self.ingress
            .update_value_with_effective_canister_id(
                management_canister(),
                "upload_chunk",
                &args,
                args.canister_id,
            )
            .await
            .map_err(|error| transport_or_reject("upload_chunk", error.to_string()))
    }

    async fn install_chunked_code(
        &self,
        args: InstallChunkedCodeArgs,
    ) -> Result<(), ManagementError> {
        self.unit_call(
            "install_chunked_code",
            Encode!(&args).expect("encode InstallChunkedCodeArgs"),
            args.target_canister,
        )
        .await
    }

    async fn stop_canister(&self, target: Principal) -> Result<(), ManagementError> {
        self.unit_call(
            "stop_canister",
            Encode!(&CanisterIdRecord {
                canister_id: target
            })
            .expect("encode CanisterIdRecord"),
            target,
        )
        .await
    }

    async fn start_canister(&self, target: Principal) -> Result<(), ManagementError> {
        self.unit_call(
            "start_canister",
            Encode!(&CanisterIdRecord {
                canister_id: target
            })
            .expect("encode CanisterIdRecord"),
            target,
        )
        .await
    }

    async fn canister_status(
        &self,
        target: Principal,
    ) -> Result<CanisterStatusReply, ManagementError> {
        self.ingress
            .query_value_with_effective_canister_id(
                management_canister(),
                "canister_status",
                &CanisterIdRecord {
                    canister_id: target,
                },
                target,
            )
            .await
            .map_err(|error| transport_or_reject("canister_status", error.to_string()))
    }
}

// === Init arguments =========================================================

/// JSON input form of Provision's init argument.
///
/// Schema mirror of `ProvisionInitArgs`; source of truth:
/// `crates/provision/provision.did` (ProvisionInitArgs),
/// `crates/provision/src/canister/init.rs` (server types). Principals appear as
/// text on the command line and are parsed before encoding.
///
/// ```json
/// {
///   "governance_principal": "renrz-6aaaa-aaaaa-aaabq-cai"
/// }
/// ```
///
/// Deployment grants are seeded afterwards via `grant upsert`, never at init.
#[derive(Debug, Deserialize)]
pub struct ProvisionInitArgsInput {
    /// The single governance authority established at init.
    pub governance_principal: String,
}

/// Candid mirror of `ProvisionInitArgs` (provision.did).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, serde::Deserialize)]
struct ProvisionInitArgsMirror {
    governance_principal: Principal,
}

/// Resolve the init argument bytes for one bootstrap deploy/upgrade.
///
/// Precedence rules:
/// - `--init-args-hex` always wins as the universal escape hatch (bytes forwarded verbatim).
/// - `provision` accepts `--init-args <JSON>` built through the typed mirror above; omitting
///   both flags is rejected because an unseeded Provision has no bootstrap authority.
/// - `account` takes **no** init argument (`crates/account/src/lib.rs`: `#[init] fn init()`
///   is empty), so JSON is rejected and absence yields empty bytes.
pub fn load_init_args(
    kind: BootstrapKind,
    init_args_json: Option<&str>,
    init_args_hex: Option<&str>,
) -> Result<Vec<u8>, String> {
    match kind {
        BootstrapKind::Account => {
            if let Some(json) = init_args_json {
                return Err(format!(
                    "account init takes no arguments (crates/account/src/lib.rs declares \
                     an empty `#[init] fn init()`); remove --init-args {json:?}"
                ));
            }
            match init_args_hex {
                Some(hex) => crate::encoding::parse_hex_blob(hex),
                None => Ok(Vec::new()),
            }
        }
        BootstrapKind::Provision => {
            if let Some(hex) = init_args_hex {
                return crate::encoding::parse_hex_blob(hex);
            }
            let Some(json) = init_args_json else {
                return Err(
                    "provision requires --init-args <JSON> (governance_principal establishes the \
                     authority) or --init-args-hex <HEX>"
                        .to_owned(),
                );
            };
            encode_provision_init_args(json)
        }
    }
}

fn encode_provision_init_args(json: &str) -> Result<Vec<u8>, String> {
    let input: ProvisionInitArgsInput = serde_json::from_str(json)
        .map_err(|error| format!("parse --init-args as ProvisionInitArgs JSON: {error}"))?;
    let governance = Principal::from_text(&input.governance_principal)
        .map_err(|error| format!("invalid governance_principal: {error}"))?;
    // Mirrors the init-time trap (crates/provision/src/canister/init.rs): reject early
    // so the failure surfaces before any management-canister call.
    if governance == Principal::anonymous() {
        return Err("anonymous governance_principal is not allowed".to_owned());
    }
    let mirror = ProvisionInitArgsMirror {
        governance_principal: governance,
    };
    Encode!(&mirror).map_err(|error| format!("encode provision init args: {error}"))
}

// === Local planning =========================================================

/// Split wasm bytes into management-canister chunk-store chunks (≤[`MAX_CHUNK_BYTES`]).
fn split_chunks(wasm: &[u8]) -> Vec<&[u8]> {
    let max = gleaph_artifact_api::types::MAX_CHUNK_BYTES;
    if wasm.is_empty() {
        return vec![&[]];
    }
    wasm.chunks(max).collect()
}

/// SHA-256 over the exact local bytes (verification material for `module_hash` comparison).
fn sha256_hex(bytes: &[u8]) -> String {
    to_hex(&sha256_bytes(bytes))
}

fn sha256_bytes(bytes: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).to_vec()
}

fn short_sha(bytes: &[u8]) -> String {
    let mut shortened: String = to_hex(bytes).chars().take(16).collect();
    shortened.push('…');
    shortened
}

/// One rendered execution step of a plan.
struct Step(String);

/// Rendered execution plan — identical text for dry-runs and confirmed executions.
struct Plan {
    header: String,
    steps: Vec<Step>,
}

impl Plan {
    fn render(&self) -> String {
        let mut text = String::new();
        text.push_str(&self.header);
        text.push('\n');
        for (index, step) in self.steps.iter().enumerate() {
            text.push_str(&format!("  {}. {}\n", index + 1, step.0));
        }
        text
    }

    fn print(&self) {
        print!("{}", self.render());
    }
}

/// Inputs of a bootstrap deploy, resolved from CLI arguments.
pub struct DeployRequest {
    /// Which platform canister is being deployed.
    pub kind: BootstrapKind,
    /// Wasm bytes read from `--wasm`.
    pub wasm: Vec<u8>,
    /// Candid-encoded init argument ([`load_init_args`]).
    pub init_arg_bytes: Vec<u8>,
    /// How the init argument was supplied, for the plan display.
    pub init_arg_source: String,
    /// Controllers assigned at creation: `[governance caller]`.
    pub controllers: Vec<Principal>,
    /// Cycles attached to `create_canster`.
    pub cycles: u128,
    /// Execute when true; otherwise only the plan is printed.
    pub confirm: bool,
}

/// Inputs of a bootstrap upgrade, resolved from CLI arguments.
pub struct UpgradeRequest {
    /// Which platform canister is being upgraded.
    pub kind: BootstrapKind,
    /// Target canister principal.
    pub target: Principal,
    /// Wasm bytes read from `--wasm`.
    pub wasm: Vec<u8>,
    /// Candid-encoded init argument ([`load_init_args`]); passed to `post_upgrade`.
    pub init_arg_bytes: Vec<u8>,
    /// How the init argument was supplied, for the plan display.
    pub init_arg_source: String,
    /// Execute when true; otherwise only the plan is printed.
    pub confirm: bool,
}

fn describe_init_arg(source: &str, bytes: &[u8]) -> String {
    if bytes.is_empty() {
        "none".to_owned()
    } else {
        format!("{} bytes ({source})", bytes.len())
    }
}

fn deploy_plan(request: &DeployRequest, wasm_sha256: &str, chunk_count: usize) -> Plan {
    Plan {
        header: format!(
            "deploy plan for {} (wasm {} bytes, sha256:{wasm_sha256})",
            request.kind.name(),
            request.wasm.len()
        ),
        steps: vec![
            Step(format!(
                "create_canister controllers=[{}] cycles={}",
                request
                    .controllers
                    .iter()
                    .map(|p| p.to_text())
                    .collect::<Vec<_>>()
                    .join(", "),
                request.cycles
            )),
            Step(format!(
                "upload_chunk × {chunk_count} (≤{} bytes each) into the new canister's chunk store",
                gleaph_artifact_api::types::MAX_CHUNK_BYTES
            )),
            Step(format!(
                "install_chunked_code mode=install init_arg={}",
                describe_init_arg(&request.init_arg_source, &request.init_arg_bytes)
            )),
            Step("canister_status — verify module_hash equals the local wasm sha256".to_owned()),
        ],
    }
}

fn upgrade_plan(
    request: &UpgradeRequest,
    wasm_sha256: &str,
    chunk_count: usize,
    current_module_hash: Option<&[u8]>,
) -> Plan {
    Plan {
        header: format!(
            "upgrade plan for {} @ {} (wasm {} bytes, sha256:{wasm_sha256})\ncurrent module_hash: {}",
            request.kind.name(),
            request.target,
            request.wasm.len(),
            current_module_hash
                .map(short_sha)
                .unwrap_or_else(|| "none".to_owned()),
        ),
        steps: vec![
            Step("stop_canister".to_owned()),
            Step(format!(
                "upload_chunk × {chunk_count} (≤{} bytes each) into the target's chunk store",
                gleaph_artifact_api::types::MAX_CHUNK_BYTES
            )),
            Step(format!(
                "install_chunked_code mode=upgrade init_arg={}",
                describe_init_arg(&request.init_arg_source, &request.init_arg_bytes)
            )),
            Step("start_canister (skipped — target left stopped — if any step fails)".to_owned()),
            Step("canister_status — compare new module_hash with the local wasm sha256".to_owned()),
        ],
    }
}

fn print_hash_comparison(label: &str, local_sha256: &str, module_hash: Option<&[u8]>) {
    let remote = module_hash.map(to_hex).unwrap_or_else(|| "none".to_owned());
    println!("{label}:");
    println!("  local wasm sha256: {local_sha256}");
    println!("  module_hash:       {remote}");
    println!(
        "  match: {}",
        module_hash.is_some_and(|hash| to_hex(hash) == local_sha256)
    );
}

// === Execution ==============================================================

/// Run `bootstrap deploy` (plan-only unless `request.confirm`).
///
/// Returns the freshly created canister id.
pub async fn execute_deploy<T: ManagementTransport + Sync>(
    transport: &T,
    request: &DeployRequest,
) -> Result<Principal, OperatorError> {
    let wasm_sha256 = sha256_hex(&request.wasm);
    let chunks = split_chunks(&request.wasm);
    let plan = deploy_plan(request, &wasm_sha256, chunks.len());
    plan.print();
    if !request.confirm {
        println!("plan only — pass --yes to execute");
        return Ok(Principal::anonymous());
    }

    let created = transport
        .create_canister(
            CreateCanisterArgs {
                settings: Some(CanisterSettings {
                    controllers: Some(request.controllers.clone()),
                    ..CanisterSettings::default()
                }),
                sender_canister_version: None,
            },
            request.cycles,
        )
        .await?;
    println!("created canister {created}");

    for (index, chunk) in chunks.iter().enumerate() {
        let hash = transport
            .upload_chunk(UploadChunkArgs {
                canister_id: created,
                chunk: chunk.to_vec(),
            })
            .await?;
        println!(
            "chunk {}/{} uploaded ({})",
            index + 1,
            chunks.len(),
            short_sha(&hash.hash)
        );
    }

    transport
        .install_chunked_code(InstallChunkedCodeArgs {
            mode: CanisterInstallMode::Install,
            target_canister: created,
            store_canister: None,
            chunk_hashes_list: chunks.iter().map(|chunk| chunk_hash(chunk)).collect(),
            wasm_module_hash: sha256_bytes(&request.wasm),
            arg: request.init_arg_bytes.clone(),
            sender_canister_version: None,
        })
        .await?;
    println!("installed {} wasm (mode=install)", request.kind.name());

    let status = transport.canister_status(created).await?;
    print_hash_comparison(
        "deploy verification",
        &wasm_sha256,
        status.module_hash.as_deref(),
    );
    Ok(created)
}

/// Chunk hash as the management canister computes it: SHA-256 over the chunk bytes.
fn chunk_hash(chunk: &[u8]) -> ChunkHash {
    ChunkHash {
        hash: sha256_bytes(chunk),
    }
}

/// Run `bootstrap upgrade` (plan-only unless `request.confirm`).
pub async fn execute_upgrade<T: ManagementTransport + Sync>(
    transport: &T,
    request: &UpgradeRequest,
) -> Result<(), OperatorError> {
    // Preflight: proves reachability + controller authorization and supplies the current
    // module hash for the verification table.
    let before = transport.canister_status(request.target).await?;
    let wasm_sha256 = sha256_hex(&request.wasm);
    let chunks = split_chunks(&request.wasm);
    let plan = upgrade_plan(
        request,
        &wasm_sha256,
        chunks.len(),
        before.module_hash.as_deref(),
    );
    plan.print();
    if !request.confirm {
        println!("plan only — pass --yes to execute");
        return Ok(());
    }

    // A canister that was already stopped before this run is restored to stopped afterwards
    // (least surprise): neither stop nor start is issued for it.
    let was_running = before.status != ManagementStatusKind::Stopped;
    let outcome = run_upgrade_steps(transport, request, &chunks, was_running).await;

    match outcome {
        Ok(()) => {
            if was_running {
                transport.start_canister(request.target).await?;
                println!("started canister {}", request.target);
            } else {
                println!(
                    "left {} stopped (it was already stopped before this upgrade)",
                    request.target
                );
            }
        }
        Err(error) => {
            eprintln!(
                "upgrade failed: {error}\n{} was NOT restarted and remains stopped; fix the \
                 cause and re-run the same command (chunk uploads are idempotent, \
                 install_chunked_code replaces the module wholesale)",
                request.target
            );
            // Best-effort confirmation of the stopped state; never masks the primary error.
            if let Ok(status) = transport.canister_status(request.target).await {
                println!("current status: {}", status.status.name());
            }
            return Err(error);
        }
    }

    let after = transport.canister_status(request.target).await?;
    print_hash_comparison(
        "upgrade verification",
        &wasm_sha256,
        after.module_hash.as_deref(),
    );
    Ok(())
}

async fn run_upgrade_steps<T: ManagementTransport + Sync>(
    transport: &T,
    request: &UpgradeRequest,
    chunks: &[&[u8]],
    was_running: bool,
) -> Result<(), OperatorError> {
    if was_running {
        transport.stop_canister(request.target).await?;
        println!("stopped canister {}", request.target);
    }
    for (index, chunk) in chunks.iter().enumerate() {
        let hash = transport
            .upload_chunk(UploadChunkArgs {
                canister_id: request.target,
                chunk: chunk.to_vec(),
            })
            .await?;
        println!(
            "chunk {}/{} uploaded ({})",
            index + 1,
            chunks.len(),
            short_sha(&hash.hash)
        );
    }
    transport
        .install_chunked_code(InstallChunkedCodeArgs {
            mode: CanisterInstallMode::Upgrade(None),
            target_canister: request.target,
            store_canister: None,
            chunk_hashes_list: chunks.iter().map(|chunk| chunk_hash(chunk)).collect(),
            wasm_module_hash: sha256_bytes(&request.wasm),
            arg: request.init_arg_bytes.clone(),
            sender_canister_version: None,
        })
        .await?;
    println!("installed upgraded wasm (mode=upgrade)");
    Ok(())
}

/// Run `bootstrap status`: print state, cycles, and module hash.
pub async fn execute_status<T: ManagementTransport + Sync>(
    transport: &T,
    target: Principal,
) -> Result<(), OperatorError> {
    let status = transport.canister_status(target).await?;
    println!("canister {target}");
    println!(
        "status={} version={}",
        status.status.name(),
        status
            .version
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_owned())
    );
    println!(
        "module_hash={}",
        status
            .module_hash
            .as_deref()
            .map(to_hex)
            .unwrap_or_else(|| "none".to_owned())
    );
    let nat_text = |value: &Option<candid::Nat>| {
        value
            .as_ref()
            .map(|nat| nat.to_string())
            .unwrap_or_else(|| "unknown".to_owned())
    };
    println!(
        "cycles={} reserved_cycles={}",
        nat_text(&status.cycles),
        nat_text(&status.reserved_cycles)
    );
    println!(
        "idle_cycles_burned_per_day={}",
        nat_text(&status.idle_cycles_burned_per_day)
    );
    println!(
        "controllers={} memory_size={}",
        status
            .settings
            .as_ref()
            .and_then(|settings| settings.controllers.as_ref())
            .map(|controllers| controllers.len())
            .unwrap_or(0),
        nat_text(&status.memory_size)
    );
    Ok(())
}

/// Entry point wired into [`crate::run`]: connect through the shared ingress layer and run
/// one bootstrap command. Bootstrap commands never touch `--provision`; their destination is
/// always the management canister.
pub async fn execute(
    command: crate::cli::BootstrapCommand,
    network: &str,
    identity: Option<&std::path::Path>,
) -> Result<(), OperatorError> {
    use crate::cli::BootstrapCommand as Cmd;

    // Fail fast on the one configuration this transport stack cannot express today
    // (see module docs): mainnet create_canister requires attached cycles.
    if matches!(
        &command,
        Cmd::Deploy(args) if args.cycles.unwrap_or(DEFAULT_CREATE_CYCLES) > 0
    ) && network == "ic"
    {
        return Err(OperatorError::Message(
            "bootstrap deploy on the \"ic\" network is unavailable: ic-agent 0.49.2 cannot \
             attach cycles to create_canister (the creation fee is charged from attached \
             cycles), so the replica would reject the call. Deploy from a fee-free endpoint \
             (-n local or an explicit http(s) URL) or attach cycles once the agent gains \
             support."
                .to_owned(),
        ));
    }

    let ingress = IcIngress::connect(network, identity).await?;
    let client = ManagementClient::new(&ingress);

    match command {
        Cmd::Deploy(args) => {
            let wasm = read_wasm(&args.wasm)?;
            let init_arg_bytes = load_init_args(
                args.kind,
                args.init_args.init_args.as_deref(),
                args.init_args.init_args_hex.as_deref(),
            )?;
            let request = DeployRequest {
                kind: args.kind,
                wasm,
                init_arg_source: init_arg_source_label(
                    args.init_args.init_args.as_deref(),
                    args.init_args.init_args_hex.as_deref(),
                ),
                init_arg_bytes,
                controllers: vec![ingress.principal()],
                cycles: args.cycles.unwrap_or(DEFAULT_CREATE_CYCLES),
                confirm: args.yes,
            };
            execute_deploy(&client, &request).await?;
        }
        Cmd::Upgrade(args) => {
            let target = parse_target(&args.target)?;
            let wasm = read_wasm(&args.wasm)?;
            let init_arg_bytes = load_init_args(
                args.kind,
                args.init_args.init_args.as_deref(),
                args.init_args.init_args_hex.as_deref(),
            )?;
            let request = UpgradeRequest {
                kind: args.kind,
                target,
                wasm,
                init_arg_source: init_arg_source_label(
                    args.init_args.init_args.as_deref(),
                    args.init_args.init_args_hex.as_deref(),
                ),
                init_arg_bytes,
                confirm: args.yes,
            };
            execute_upgrade(&client, &request).await?;
        }
        Cmd::Status(args) => {
            let target = parse_target(&args.target)?;
            execute_status(&client, target).await?;
        }
    }
    Ok(())
}

fn init_arg_source_label(json: Option<&str>, hex: Option<&str>) -> String {
    if hex.is_some() {
        "--init-args-hex".to_owned()
    } else if json.is_some() {
        "--init-args JSON mirror".to_owned()
    } else {
        "default".to_owned()
    }
}

fn read_wasm(path: &std::path::Path) -> Result<Vec<u8>, OperatorError> {
    std::fs::read(path)
        .map_err(|error| OperatorError::Message(format!("read wasm {}: {error}", path.display())))
}

fn parse_target(text: &str) -> Result<Principal, OperatorError> {
    Principal::from_text(text)
        .map_err(|error| OperatorError::Message(format!("invalid --target principal: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Decode;
    use std::sync::Mutex;

    fn chunk_upload_event(bytes: usize) -> String {
        format!("upload_chunk({bytes} bytes)")
    }

    const MAX_CHUNK_BYTES: usize = gleaph_artifact_api::types::MAX_CHUNK_BYTES;

    // -- Fake transport ------------------------------------------------------

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
        fail_method: Option<&'static str>,
        stopped: Mutex<bool>,
        module_hash: Mutex<Option<Vec<u8>>>,
    }

    impl Recorder {
        fn record(&self, event: String) {
            self.calls.lock().unwrap().push(event);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn fake_status(stopped: bool, module_hash: Option<Vec<u8>>) -> CanisterStatusReply {
        CanisterStatusReply {
            status: if stopped {
                ManagementStatusKind::Stopped
            } else {
                ManagementStatusKind::Running
            },
            module_hash,
            version: Some(1),
            cycles: Some(0u64.into()),
            reserved_cycles: Some(0u64.into()),
            idle_cycles_burned_per_day: Some(0u64.into()),
            memory_size: Some(0u64.into()),
            settings: Some(crate::wire::ManagementDefiniteCanisterSettings {
                controllers: Some(Vec::new()),
            }),
        }
    }

    impl ManagementTransport for Recorder {
        async fn create_canister(
            &self,
            _args: CreateCanisterArgs,
            cycles: u128,
        ) -> Result<Principal, ManagementError> {
            self.record(format!("create_canister(cycles={cycles})"));
            if self.fail_method == Some("create_canister") {
                return Err(reject("create_canister"));
            }
            Ok(Principal::from_slice(&[7; 29]))
        }

        async fn upload_chunk(&self, args: UploadChunkArgs) -> Result<ChunkHash, ManagementError> {
            self.record(format!("upload_chunk({} bytes)", args.chunk.len()));
            if self.fail_method == Some("upload_chunk") {
                return Err(reject("upload_chunk"));
            }
            Ok(ChunkHash {
                hash: vec![args.chunk.len() as u8],
            })
        }

        async fn install_chunked_code(
            &self,
            args: InstallChunkedCodeArgs,
        ) -> Result<(), ManagementError> {
            self.record(format!("install_chunked_code({})", mode_name(args.mode)));
            if self.fail_method == Some("install_chunked_code") {
                return Err(reject("install_chunked_code"));
            }
            *self.module_hash.lock().unwrap() = Some(args.wasm_module_hash.clone());
            Ok(())
        }

        async fn stop_canister(&self, _target: Principal) -> Result<(), ManagementError> {
            self.record("stop_canister".to_owned());
            if self.fail_method == Some("stop_canister") {
                return Err(reject("stop_canister"));
            }
            *self.stopped.lock().unwrap() = true;
            Ok(())
        }

        async fn start_canister(&self, _target: Principal) -> Result<(), ManagementError> {
            self.record("start_canister".to_owned());
            if self.fail_method == Some("start_canister") {
                return Err(reject("start_canister"));
            }
            *self.stopped.lock().unwrap() = false;
            Ok(())
        }

        async fn canister_status(
            &self,
            _target: Principal,
        ) -> Result<CanisterStatusReply, ManagementError> {
            self.record("canister_status".to_owned());
            Ok(fake_status(
                *self.stopped.lock().unwrap(),
                self.module_hash.lock().unwrap().clone(),
            ))
        }
    }

    fn mode_name(mode: CanisterInstallMode) -> &'static str {
        match mode {
            CanisterInstallMode::Install => "install",
            CanisterInstallMode::Reinstall => "reinstall",
            CanisterInstallMode::Upgrade(_) => "upgrade",
        }
    }

    fn reject(method: &'static str) -> ManagementError {
        ManagementError::Reject {
            method,
            reason: "synthetic failure".to_owned(),
        }
    }

    fn multi_chunk_wasm() -> Vec<u8> {
        let size = gleaph_artifact_api::types::MAX_CHUNK_BYTES * 2 + 11;
        (0..size).map(|i| (i % 253) as u8).collect()
    }

    fn deploy_request(confirm: bool, wasm: Vec<u8>) -> DeployRequest {
        DeployRequest {
            kind: BootstrapKind::Provision,
            wasm,
            init_arg_bytes: vec![1, 2, 3],
            init_arg_source: "--init-args-hex".to_owned(),
            controllers: vec![Principal::from_slice(&[0xAB; 29])],
            cycles: DEFAULT_CREATE_CYCLES,
            confirm,
        }
    }

    fn upgrade_request(confirm: bool, wasm: Vec<u8>) -> UpgradeRequest {
        UpgradeRequest {
            kind: BootstrapKind::Provision,
            target: Principal::from_slice(&[9; 29]),
            wasm,
            init_arg_bytes: Vec::new(),
            init_arg_source: "default".to_owned(),
            confirm,
        }
    }

    fn expected_chunks(wasm: &[u8]) -> usize {
        wasm.chunks(gleaph_artifact_api::types::MAX_CHUNK_BYTES)
            .count()
    }

    // -- Orchestration -------------------------------------------------------

    #[test]
    fn deploy_executes_create_upload_install_then_verifies() {
        let wasm = multi_chunk_wasm();
        let transport = Recorder::default();
        let created = pollster::block_on(execute_deploy(
            &transport,
            &deploy_request(true, wasm.clone()),
        ))
        .expect("deploy");
        assert_eq!(created, Principal::from_slice(&[7; 29]));
        assert_eq!(
            transport.calls(),
            vec![
                "create_canister(cycles=1000000000000)".to_owned(),
                chunk_upload_event(MAX_CHUNK_BYTES),
                chunk_upload_event(MAX_CHUNK_BYTES),
                "upload_chunk(11 bytes)".to_owned(),
                "install_chunked_code(install)".to_owned(),
                "canister_status".to_owned(),
            ]
        );
        // The installed module hash equals the local wasm sha256 (verification material).
        let installed = transport.module_hash.lock().unwrap().clone().unwrap();
        assert_eq!(to_hex(&installed), sha256_hex(&wasm));
    }

    #[test]
    fn deploy_without_confirm_prints_plan_and_touches_nothing() {
        let transport = Recorder::default();
        let created = pollster::block_on(execute_deploy(
            &transport,
            &deploy_request(false, multi_chunk_wasm()),
        ))
        .expect("dry-run deploy");
        assert_eq!(created, Principal::anonymous(), "no canister is created");
        assert!(
            transport.calls().is_empty(),
            "dry run must not call: {:?}",
            transport.calls()
        );
    }

    #[test]
    fn upgrade_runs_stop_upload_install_start_in_order() {
        let wasm = multi_chunk_wasm();
        let transport = Recorder::default();
        pollster::block_on(execute_upgrade(&transport, &upgrade_request(true, wasm)))
            .expect("upgrade");
        assert_eq!(
            transport.calls(),
            vec![
                "canister_status".to_owned(),
                "stop_canister".to_owned(),
                chunk_upload_event(MAX_CHUNK_BYTES),
                chunk_upload_event(MAX_CHUNK_BYTES),
                "upload_chunk(11 bytes)".to_owned(),
                "install_chunked_code(upgrade)".to_owned(),
                "start_canister".to_owned(),
                "canister_status".to_owned(),
            ]
        );
    }

    #[test]
    fn upgrade_without_confirm_only_preflights_status() {
        let transport = Recorder::default();
        pollster::block_on(execute_upgrade(
            &transport,
            &upgrade_request(false, multi_chunk_wasm()),
        ))
        .expect("dry-run upgrade");
        assert_eq!(transport.calls(), vec!["canister_status"]);
    }

    #[test]
    fn failed_install_leaves_the_target_stopped_without_restart() {
        let transport = Recorder {
            fail_method: Some("install_chunked_code"),
            ..Recorder::default()
        };
        let error = pollster::block_on(execute_upgrade(
            &transport,
            &upgrade_request(true, multi_chunk_wasm()),
        ))
        .expect_err("install failure must abort");
        assert!(error.to_string().contains("install_chunked_code"));
        let calls = transport.calls();
        assert!(
            !calls.contains(&"start_canister".to_owned()),
            "auto-start forbidden: {calls:?}"
        );
        // Exactly two status probes: preflight + best-effort failure report. The final
        // verification probe is never reached on the failure path.
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "canister_status")
                .count(),
            2
        );
        assert!(
            *transport.stopped.lock().unwrap(),
            "target must remain stopped"
        );
    }

    #[test]
    fn failed_stop_aborts_before_any_byte_is_uploaded() {
        let transport = Recorder {
            fail_method: Some("stop_canister"),
            ..Recorder::default()
        };
        let error = pollster::block_on(execute_upgrade(
            &transport,
            &upgrade_request(true, multi_chunk_wasm()),
        ))
        .expect_err("stop failure must abort");
        assert!(error.to_string().contains("stop_canister"), "{error}");
        let calls = transport.calls();
        assert!(
            !calls.iter().any(|call| call.starts_with("upload_chunk")),
            "no upload may follow a failed stop: {calls:?}"
        );
        assert!(!calls.contains(&"install_chunked_code(upgrade)".to_owned()));
        assert!(!calls.contains(&"start_canister".to_owned()));
    }

    #[test]
    fn already_stopped_target_skips_stop_and_restore() {
        let transport = Recorder::default();
        *transport.stopped.lock().unwrap() = true;
        pollster::block_on(execute_upgrade(
            &transport,
            &upgrade_request(true, multi_chunk_wasm()),
        ))
        .expect("upgrade of stopped target");
        let calls = transport.calls();
        assert!(!calls.contains(&"stop_canister".to_owned()), "{calls:?}");
        assert!(
            !calls.contains(&"start_canister".to_owned()),
            "prior state (stopped) is restored"
        );
        assert!(calls.contains(&"install_chunked_code(upgrade)".to_owned()));
    }

    // -- Planning ------------------------------------------------------------

    #[test]
    fn plans_render_every_step_for_both_operations() {
        let deploy = deploy_request(true, multi_chunk_wasm());
        let rendered = deploy_plan(&deploy, "ab".repeat(32).as_str(), 3).render();
        assert!(
            rendered.starts_with("deploy plan for provision"),
            "{rendered}"
        );
        assert!(rendered.contains("create_canister controllers=["));
        assert!(rendered.contains("cycles=1000000000000"));
        assert!(rendered.contains("upload_chunk × 3"));
        assert!(rendered.contains("mode=install"));

        let upgrade = upgrade_request(false, multi_chunk_wasm());
        let target_text = upgrade.target.to_text();
        let rendered = upgrade_plan(&upgrade, "cd".repeat(32).as_str(), 4, Some(&[9; 32])).render();
        assert!(
            rendered.starts_with("upgrade plan for provision"),
            "{rendered}"
        );
        assert!(rendered.contains(&target_text), "{rendered}");
        assert!(rendered.contains("current module_hash:"));
        assert!(rendered.contains("stop_canister"));
        assert!(rendered.contains("upload_chunk × 4"));
        assert!(rendered.contains("mode=upgrade"));
        assert!(rendered.contains("start_canister"));
    }

    // -- Chunks & hashing ----------------------------------------------------

    #[test]
    fn chunk_splitting_respects_the_one_mib_bound() {
        let wasm = multi_chunk_wasm();
        let chunks = split_chunks(&wasm);
        assert_eq!(chunks.len(), expected_chunks(&wasm));
        for chunk in &chunks {
            assert!(chunk.len() <= gleaph_artifact_api::types::MAX_CHUNK_BYTES);
        }
        assert_eq!(chunks.concat(), wasm, "splitting must be lossless");

        assert_eq!(split_chunks(b"x").len(), 1);
        assert_eq!(
            split_chunks(&[]).len(),
            1,
            "empty wasm stays a single (empty) chunk"
        );
    }

    // -- Init arguments ------------------------------------------------------

    /// Deterministic valid principal text (29 bytes of `byte`), used instead of made-up
    /// literal texts so the CRC check in `Principal::from_text` always passes.
    fn principal_text(byte: u8) -> String {
        Principal::from_slice(&[byte; 29]).to_text()
    }

    fn init_args_json() -> String {
        format!(
            r#"{{"governance_principal":"{}"}}"#,
            principal_text(0x02),
        )
    }

    #[test]
    fn provision_init_args_mirror_matches_server_wire() {
        let bytes = load_init_args(BootstrapKind::Provision, Some(&init_args_json()), None)
            .expect("encode provision init args");

        // Operator mirror bytes decode with the server's own type.
        let server: gleaph_provision::canister::init::ProvisionInitArgs =
            Decode!(&bytes, gleaph_provision::canister::init::ProvisionInitArgs)
                .expect("server decodes operator mirror");
        assert_eq!(
            server.governance_principal,
            Principal::from_slice(&[0x02; 29])
        );

        // Server-typed value encodes into something the operator mirror decodes identically.
        let server_value = gleaph_provision::canister::init::ProvisionInitArgs {
            governance_principal: Principal::from_slice(&[0x03; 29]),
        };
        let server_bytes = candid::Encode!(&server_value).expect("encode server init args");
        let decoded: ProvisionInitArgsMirror =
            Decode!(&server_bytes, ProvisionInitArgsMirror).expect("mirror decodes server");
        assert_eq!(
            decoded.governance_principal,
            Principal::from_slice(&[0x03; 29])
        );
    }

    #[test]
    fn provision_requires_explicit_init_args() {
        let error = load_init_args(BootstrapKind::Provision, None, None).expect_err("required");
        assert!(error.contains("--init-args"), "got: {error}");
    }

    #[test]
    fn account_takes_no_init_argument_but_keeps_the_hex_escape_hatch() {
        let error = load_init_args(BootstrapKind::Account, Some("{}"), None).expect_err("json");
        assert!(error.contains("takes no arguments"), "got: {error}");
        assert!(
            error.contains("crates/account/src/lib.rs"),
            "must cite the source: {error}"
        );

        assert_eq!(
            load_init_args(BootstrapKind::Account, None, None).expect("default"),
            Vec::<u8>::new()
        );
        assert_eq!(
            load_init_args(BootstrapKind::Account, None, Some("deadbeef")).expect("hex"),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn provision_init_args_validate_principals_and_anonymous_governance() {
        let bad_principal = init_args_json().replace(&principal_text(0x02), "not-a-principal");
        let error = load_init_args(BootstrapKind::Provision, Some(&bad_principal), None)
            .expect_err("bad principal");
        assert!(error.contains("governance_principal"), "got: {error}");

        let anon =
            init_args_json().replace(&principal_text(0x02), &Principal::anonymous().to_text());
        let error = load_init_args(BootstrapKind::Provision, Some(&anon), None)
            .expect_err("anonymous governance");
        assert!(
            error.contains("anonymous governance_principal"),
            "got: {error}"
        );
    }

    #[test]
    fn hex_escape_hatch_overrides_json_for_provision() {
        let bytes = load_init_args(BootstrapKind::Provision, Some(&init_args_json()), Some("00"))
            .expect("hex wins");
        assert_eq!(bytes, vec![0x00]);
    }
}
