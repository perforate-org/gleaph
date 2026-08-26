//! Command → logic mapping and output printing.
//!
//! Every `run_*` function owns exactly one CLI command: it converts parsed arguments into
//! library calls ([`gleaph_artifact_api`] planning/driver plus [`crate::transport`]), maps
//! failures into [`OperatorError`], and prints the result. No protocol logic lives here —
//! chunking, ordering, and resume belong to the shared driver.

use std::path::{Path, PathBuf};

use gleaph_artifact_api::driver::{IngestError, IngestOutcome, ingest_artifact};
use gleaph_artifact_api::pipeline::plan_artifact;
use gleaph_artifact_api::types::{ArtifactId, ArtifactUploadState, ReleaseActivateArgs, ReleaseId};

use crate::cli::{
    ArtifactCommand, ArtifactIngestArgs, ArtifactStatusArgs, AuditCommand, BindingCommand,
    BindingInstallArgs, CanisterCommand, CanisterInstallArgs, Command, ReleaseActivateCliArgs,
    ReleaseCommand, ReleasePublishCliArgs,
};
use crate::encoding::{
    artifact_id_label, kind_name, parse_hex_blob, parse_kind, parse_sha256_hex, to_hex,
};
use crate::error::OperatorError;
use crate::manifest::load_release_manifest;
use crate::transport::{IcIngress, ProvisionClient};
use crate::wire::{
    AdminInstallDeploymentBindingArgs, ArtifactAuditAction, ArtifactAuditOutcome,
    ReleaseInstallArgs,
};

/// How many times `artifact ingest` polls a still-verifying artifact before handing control
/// back to the operator (who can re-run `artifact status`). Server-side verification normally
/// completes inside the final upload call; the loop only covers the resumed-fully-chunked edge.
const VERIFICATION_POLLS: u32 = 10;
/// Delay between verification polls.
const VERIFICATION_POLL_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// Resolved connection target for one invocation.
pub struct Connection {
    provision: Option<candid::Principal>,
    network: String,
    identity: Option<PathBuf>,
}

impl Connection {
    /// Resolve and validate connection flags.
    ///
    /// Fails closed when `requires_provision` is set but `--provision` is absent or not a
    /// principal. Bootstrap-tier commands target the management canister directly and pass
    /// `false` — they neither need nor accept `--provision`.
    pub fn resolve(
        provision: Option<&str>,
        network: &str,
        identity: Option<&Path>,
        requires_provision: bool,
    ) -> Result<Self, OperatorError> {
        let provision = match provision {
            Some(text) => Some(
                candid::Principal::from_text(text)
                    .map_err(|error| format!("invalid --provision principal: {error}"))?,
            ),
            None if requires_provision => {
                return Err(
                    "missing --provision <PRINCIPAL>; every operator command targets \
                     one Provision canister"
                        .to_owned()
                        .into(),
                );
            }
            None => None,
        };
        Ok(Self {
            provision,
            network: network.to_owned(),
            identity: identity.map(Path::to_path_buf),
        })
    }

    /// The Provision canister this invocation targets.
    fn provision(&self) -> candid::Principal {
        self.provision
            .expect("resolve enforces --provision for data-plane commands")
    }

    /// Network selector or endpoint URL of this invocation.
    pub fn network(&self) -> &str {
        &self.network
    }

    /// PEM identity path of this invocation, when given.
    pub fn identity(&self) -> Option<&Path> {
        self.identity.as_deref()
    }

    /// Open the ingress layer with this invocation's network/identity conventions.
    async fn connect_ingress(&self) -> Result<IcIngress, OperatorError> {
        IcIngress::connect(&self.network, self.identity.as_deref())
            .await
            .map_err(OperatorError::Message)
    }
}

/// Dispatch one parsed command against the resolved connection.
///
/// # Panics
/// Only through the transport's mid-driver failure policy (see
/// [`crate::transport`] module docs); ordinary failures return [`OperatorError`].
#[allow(clippy::too_many_lines)] // flat dispatch keeps each arm's mapping visible
pub async fn execute(command: Command, connection: &Connection) -> Result<(), OperatorError> {
    match command {
        Command::Artifact(ArtifactCommand::Ingest(args)) => run_ingest(connection, args).await,
        Command::Artifact(ArtifactCommand::Status(args)) => run_status(connection, args).await,
        Command::Release(ReleaseCommand::Publish(args)) => {
            run_release_publish(connection, args).await
        }
        Command::Release(ReleaseCommand::Activate(args)) => {
            run_release_activate(connection, args).await
        }
        Command::Release(ReleaseCommand::GetActive) => run_release_get_active(connection).await,
        Command::Canister(CanisterCommand::Install(args)) => {
            run_canister_install(connection, args).await
        }
        Command::Binding(BindingCommand::Install(args)) => {
            run_binding_install(connection, args).await
        }
        Command::Audit(AuditCommand::History) => run_audit_history(connection).await,
        // Bootstrap tier (ADR 0087 §Explicitly deferred): management-canister destination,
        // so it uses the connection's network/identity but never `--provision`.
        Command::Bootstrap(command) => {
            crate::bootstrap::execute(command, connection.network(), connection.identity()).await
        }
    }
}

fn parse_identity_triple(
    kind: &str,
    version: &str,
    sha256: &str,
) -> Result<ArtifactId, OperatorError> {
    Ok(ArtifactId::new(
        parse_kind(kind)?,
        version.to_owned(),
        parse_sha256_hex(sha256)?,
    ))
}

fn parse_principal(flag: &str, text: &str) -> Result<candid::Principal, OperatorError> {
    candid::Principal::from_text(text).map_err(|error| {
        OperatorError::Message(format!("invalid {flag} principal {text:?}: {error}"))
    })
}

/// `artifact ingest`: plan locally, preflight the connection, then hand the plan to the
/// shared idempotent driver.
async fn run_ingest(
    connection: &Connection,
    args: ArtifactIngestArgs,
) -> Result<(), OperatorError> {
    let bytes = std::fs::read(&args.wasm).map_err(|error| {
        OperatorError::Message(format!("read wasm {}: {error}", args.wasm.display()))
    })?;
    let kind = parse_kind(&args.kind)?;
    let plan = plan_artifact(&bytes, kind, &args.version)?;
    println!("planned artifact: {}", artifact_id_label(&plan.artifact_id));
    println!(
        "byte_length={} chunks={} (max {} bytes each)",
        plan.byte_length(),
        plan.chunk_count(),
        gleaph_artifact_api::types::MAX_CHUNK_BYTES
    );

    let ingress = connection.connect_ingress().await?;
    let mut client = ProvisionClient::new(&ingress, connection.provision());
    // Typed preflight: surfaces endpoint/identity/authorization problems through the normal
    // error channel before the driver takes over.
    let _preflight = client.artifact_status(plan.artifact_id.clone()).await??;

    if plan.chunk_count() > 1 {
        let total = plan.chunk_count();
        client.set_on_chunk_uploaded(std::sync::Arc::new(move |chunk_index| {
            println!("chunk {}/{total} uploaded", chunk_index + 1);
        }));
    }

    match ingest_artifact(&plan, &client).await? {
        IngestOutcome::Verified { verified_at_ns } => {
            print_verified(&plan.artifact_id, verified_at_ns);
            Ok(())
        }
        IngestOutcome::AwaitingVerification { artifact_id } => {
            poll_verification(&client, &artifact_id).await
        }
    }
}

/// Poll a fully-chunked-but-unverified artifact until the server reports a terminal state.
async fn poll_verification(
    client: &ProvisionClient<'_>,
    artifact_id: &ArtifactId,
) -> Result<(), OperatorError> {
    for _ in 0..VERIFICATION_POLLS {
        tokio::time::sleep(VERIFICATION_POLL_DELAY).await;
        let status = client.artifact_status(artifact_id.clone()).await??;
        match status {
            // Verified uploads reclaim their row: `None` means done.
            None => {
                print_verified(artifact_id, None);
                return Ok(());
            }
            Some(upload) => match upload.state {
                ArtifactUploadState::Verified { verified_at_ns } => {
                    print_verified(artifact_id, Some(verified_at_ns));
                    return Ok(());
                }
                ArtifactUploadState::Failed { reason } => {
                    return Err(IngestError::UploadFailed { reason }.into());
                }
                ArtifactUploadState::Receiving | ArtifactUploadState::Verifying => continue,
            },
        }
    }
    println!(
        "verification is still running server-side after {} polls; re-run \
         `gleaph-operator artifact status --kind {} --version {} --sha256 {}` later",
        VERIFICATION_POLLS,
        kind_name(artifact_id.canister_kind),
        artifact_id.semantic_version,
        to_hex(&artifact_id.sha256)
    );
    Ok(())
}

fn print_verified(artifact_id: &ArtifactId, verified_at_ns: Option<u64>) {
    match verified_at_ns {
        Some(at_ns) => println!("verified (verified_at_ns={at_ns})"),
        None => println!("verified (upload row reclaimed by verification)"),
    }
    println!("artifact id: {}", artifact_id_label(artifact_id));
}

/// `artifact status`: report the server-side upload state for an identity triple.
async fn run_status(
    connection: &Connection,
    args: ArtifactStatusArgs,
) -> Result<(), OperatorError> {
    let artifact_id = parse_identity_triple(&args.kind, &args.version, &args.sha256)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let status = client.artifact_status(artifact_id.clone()).await??;
    match status {
        None => println!(
            "no upload row for {} — either never published, or already verified (rows are \
             reclaimed at verify); check catalog membership via `release publish` rejection \
             or re-ingest idempotently",
            artifact_id_label(&artifact_id)
        ),
        Some(upload) => {
            println!("artifact: {}", artifact_id_label(&artifact_id));
            println!("received_chunks={}", upload.received_chunks.len());
            match upload.state {
                ArtifactUploadState::Failed { reason } => {
                    println!("state=Failed reason={reason:?}");
                }
                ArtifactUploadState::Receiving => println!("state=Receiving"),
                ArtifactUploadState::Verifying => println!("state=Verifying"),
                ArtifactUploadState::Verified { verified_at_ns } => {
                    println!("state=Verified verified_at_ns={verified_at_ns}");
                }
            }
        }
    }
    Ok(())
}

/// `release publish`: load the JSON manifest and declare the release.
async fn run_release_publish(
    connection: &Connection,
    args: ReleasePublishCliArgs,
) -> Result<(), OperatorError> {
    let publish_args = load_release_manifest(&args.manifest)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let manifest = client.release_publish(publish_args).await??;
    println!(
        "published release {:?} with artifacts:",
        manifest.release_id.0
    );
    for (role, id) in [
        ("router", &manifest.router_artifact),
        ("graph", &manifest.graph_artifact),
        ("property_index", &manifest.property_index_artifact),
        ("vector_canister", &manifest.vector_canister_artifact),
        ("text_canister", &manifest.text_canister_artifact),
    ] {
        println!("  {role}: {}", artifact_id_label(id));
    }
    Ok(())
}

/// `release activate`: swap the active pointer atomically.
async fn run_release_activate(
    connection: &Connection,
    args: ReleaseActivateCliArgs,
) -> Result<(), OperatorError> {
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let result = client
        .release_activate(ReleaseActivateArgs {
            release_id: ReleaseId(args.release_id.clone()),
        })
        .await??;
    println!(
        "activated release {:?} at activated_at_ns={}",
        result.release_id.0, result.activated_at_ns
    );
    match result.previous_release_id {
        Some(previous) => println!("previous active release: {previous:?}"),
        None => println!("no previous active release"),
    }
    Ok(())
}

/// `release get-active`: read the active-release pointer.
async fn run_release_get_active(connection: &Connection) -> Result<(), OperatorError> {
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    match client.release_get_active().await? {
        Some(result) => println!(
            "active release: {:?} (activated_at_ns={}; previous: {})",
            result.release_id.0,
            result.activated_at_ns,
            result
                .previous_release_id
                .as_ref()
                .map_or("none".to_owned(), |id| format!("{:?}", id.0))
        ),
        None => println!("no active release"),
    }
    Ok(())
}

/// `canister install`: install the active release's artifact into an explicit target.
async fn run_canister_install(
    connection: &Connection,
    args: CanisterInstallArgs,
) -> Result<(), OperatorError> {
    let target = parse_principal("--target", &args.target)?;
    let kind = parse_kind(&args.kind)?;
    let install_args = parse_hex_blob(&args.install_args_hex)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let result = client
        .release_install(ReleaseInstallArgs {
            target_canister_kind: kind,
            registry_version: args.registry_version,
            install_args,
            target_canister_id: Some(target),
        })
        .await??;
    println!(
        "installed release {:?} into {} ({} chunks)",
        result.release_id.0, result.target_canister_id, result.installed_chunks
    );
    println!(
        "install_chunked_code_hash={} installed_at_ns={}",
        to_hex(&result.install_chunked_code_hash),
        result.installed_at_ns
    );
    Ok(())
}

/// `binding install`: write a deployment trust binding as governance.
async fn run_binding_install(
    connection: &Connection,
    args: BindingInstallArgs,
) -> Result<(), OperatorError> {
    let router = parse_principal("--router", &args.router)?;
    let governance = parse_principal("--governance", &args.governance)?;
    let bootstrap = match &args.bootstrap {
        Some(text) => Some(parse_principal("--bootstrap", text)?),
        None => None,
    };
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let entry = client
        .admin_install_deployment_binding(AdminInstallDeploymentBindingArgs {
            binding_version: args.binding_version,
            router_principal: router,
            governance_principal: governance,
            bootstrap_principal: bootstrap,
            deployment_id: args.deployment_id.clone(),
        })
        .await??;
    println!(
        "binding installed: deployment={:?} action={:?} binding_version={} timestamp_ns={}",
        entry.deployment_id,
        entry.action,
        entry.registry_version.unwrap_or(0),
        entry.timestamp_ns
    );
    Ok(())
}

/// `audit history`: read back the durable audit log rows visible to the caller.
async fn run_audit_history(connection: &Connection) -> Result<(), OperatorError> {
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let rows = client.artifact_audit_history().await??;
    println!("{} audit entries", rows.len());
    for (index, row) in rows.iter().enumerate() {
        print_audit_row(index, row);
    }
    Ok(())
}

fn print_audit_row(index: usize, row: &crate::wire::ArtifactAuditEntry) {
    // The wire enum names are the did variant names; rendering them verbatim keeps operator
    // output greppable against provision.did.
    let action = match row.action {
        ArtifactAuditAction::PublishArtifact => "PublishArtifact",
        ArtifactAuditAction::ActivateRelease => "ActivateRelease",
        ArtifactAuditAction::PublishRelease => "PublishRelease",
        ArtifactAuditAction::UploadChunk => "UploadChunk",
        ArtifactAuditAction::VerifyArtifact => "VerifyArtifact",
        ArtifactAuditAction::InstallRelease => "InstallRelease",
    };
    let outcome = match row.outcome {
        ArtifactAuditOutcome::Success => "Success",
        ArtifactAuditOutcome::Rejected => "Rejected",
        ArtifactAuditOutcome::Failed => "Failed",
    };
    print!(
        "[{index}] ts={} caller={} action={action} outcome={outcome}",
        row.timestamp_ns, row.caller
    );
    if let Some(id) = &row.artifact_id {
        print!(" artifact={}", artifact_id_label(id));
    }
    if let Some(release) = &row.release_id {
        print!(" release={:?}", release.0);
    }
    if let Some(target) = &row.target_canister {
        print!(" target={target}");
    }
    if let Some(deployment) = &row.deployment_id {
        print!(" deployment={deployment:?}");
    }
    if let Some(reason) = &row.reason {
        print!(" reason={reason:?}");
    }
    println!();
}
