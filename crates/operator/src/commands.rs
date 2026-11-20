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
    ArtifactCommand, ArtifactIngestArgs, ArtifactStatusArgs, AuditCommand, BillingCommand,
    BillingGrantArgs, BillingGrantStatusArgs, BillingGrantsArgs, BillingReverseArgs,
    CanisterCommand, CanisterInstallArgs, Command, DeploymentCommand, DeploymentJobArgs,
    DeploymentJobsArgs, DeploymentReconcileArgs, DeploymentRepairArgs, DeploymentResumeArgs,
    DeploymentSetOwnerArgs, DeploymentShowArgs, DeploymentSuspendArgs, DeploymentTakeOwnershipArgs,
    DeploymentTerminationArgs, DeploymentTransferSelfCustodyArgs, GrantCommand, GrantUpsertArgs,
    ReleaseActivateCliArgs, ReleaseCommand, ReleasePublishCliArgs, TreasuryCommand,
    TreasuryStatusArgs, TreasuryWithdrawArgs,
};
use crate::encoding::{
    artifact_id_label, kind_name, parse_hex_blob, parse_kind, parse_sha256_hex, to_hex,
};
use crate::error::OperatorError;
use crate::manifest::load_release_manifest;
use crate::transport::{IcIngress, ProvisionClient};
use crate::wire::{
    ArtifactAuditAction, ArtifactAuditOutcome, BillingGrantView, DeploymentGrantScope,
    FailedCanisterRepair, GrantCreditArgs, GrantCreditError, ProvisionDeploymentTermination,
    ReleaseInstallArgs, ReverseCreditArgs, ReverseCreditError, UpsertDeploymentGrantArgs,
    WithdrawArgs as WireWithdrawArgs, WithdrawError, WithdrawalKind, WithdrawalStatus,
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
#[allow(
    clippy::too_many_lines,
    reason = "a flat dispatch keeps every arm's mapping visible in one place"
)]
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
        Command::Grant(GrantCommand::Upsert(args)) => run_grant_upsert(connection, args).await,
        Command::Audit(AuditCommand::History) => run_audit_history(connection).await,
        Command::Treasury(TreasuryCommand::Ledger) => run_treasury_ledger(connection).await,
        Command::Treasury(TreasuryCommand::Withdraw(args)) => {
            run_treasury_withdraw(connection, args).await
        }
        Command::Treasury(TreasuryCommand::Status(args)) => {
            run_treasury_status(connection, args).await
        }
        Command::Billing(BillingCommand::Grant(args)) => run_billing_grant(connection, args).await,
        Command::Billing(BillingCommand::Reverse(args)) => {
            run_billing_reverse(connection, args).await
        }
        Command::Billing(BillingCommand::Grants(args)) => {
            run_billing_grants(connection, args).await
        }
        Command::Billing(BillingCommand::GrantStatus(args)) => {
            run_billing_grant_status(connection, args).await
        }
        Command::Deployment(DeploymentCommand::Show(args)) => {
            run_deployment_show(connection, args).await
        }
        Command::Deployment(DeploymentCommand::Jobs(args)) => {
            run_deployment_jobs(connection, args).await
        }
        Command::Deployment(DeploymentCommand::Job(args)) => {
            run_deployment_job(connection, args).await
        }
        Command::Deployment(DeploymentCommand::SetOwner(args)) => {
            run_deployment_set_owner(connection, args).await
        }
        Command::Deployment(DeploymentCommand::TakeOwnership(args)) => {
            run_deployment_take_ownership(connection, args).await
        }
        Command::Deployment(DeploymentCommand::TransferSelfCustody(args)) => {
            run_deployment_transfer_self_custody(connection, args).await
        }
        Command::Deployment(DeploymentCommand::Suspend(args)) => {
            run_deployment_suspend(connection, args).await
        }
        Command::Deployment(DeploymentCommand::Resume(args)) => {
            run_deployment_resume(connection, args).await
        }
        Command::Deployment(DeploymentCommand::Termination(args)) => {
            run_deployment_termination(connection, args).await
        }
        Command::Deployment(DeploymentCommand::Reconcile(args)) => {
            run_deployment_reconcile(connection, args).await
        }
        Command::Deployment(DeploymentCommand::Repair(args)) => {
            run_deployment_repair(connection, args).await
        }
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
    let _preflight = client.artifact_status(plan.artifact_id.clone()).await?;

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
        let status = client.artifact_status(artifact_id.clone()).await?;
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
    let status = client.artifact_status(artifact_id.clone()).await?;
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

/// `grant upsert`: authorize an issuer to request issuance (governance only).
async fn run_grant_upsert(
    connection: &Connection,
    args: GrantUpsertArgs,
) -> Result<(), OperatorError> {
    let issuer = parse_principal("issuer", &args.issuer)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let scope = if args.relay {
        DeploymentGrantScope::Relay
    } else {
        DeploymentGrantScope::OwnDeployment
    };
    let entry = client
        .upsert_deployment_grant(UpsertDeploymentGrantArgs { scope, issuer })
        .await??;
    println!(
        "grant upserted: deployment={:?} action={:?} timestamp_ns={}",
        entry.deployment_id, entry.action, entry.timestamp_ns
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

/// Map a Provision withdrawal rejection to the operator error channel.
fn withdraw_error(error: WithdrawError) -> OperatorError {
    OperatorError::Message(format!("withdraw rejected: {error:?}"))
}

fn parse_idempotency_key(flag: &str, text: &str) -> Result<[u8; 32], OperatorError> {
    parse_sha256_hex(text)
        .map_err(|error| OperatorError::Message(format!("invalid {flag}: {error}")))
}

/// `treasury ledger`: print the currently withheld pools.
async fn run_treasury_ledger(connection: &Connection) -> Result<(), OperatorError> {
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let ledger = client.service_ledger().await?.map_err(withdraw_error)?;
    println!("platform_fees: {}", ledger.platform_fees);
    println!("compensation_reserve: {}", ledger.compensation_reserve);
    Ok(())
}

/// `treasury withdraw`: preview unless `--yes`, then debit one pool and deposit to a canister.
async fn run_treasury_withdraw(
    connection: &Connection,
    args: TreasuryWithdrawArgs,
) -> Result<(), OperatorError> {
    let destination = parse_principal("--destination", &args.destination)?;
    let idempotency_key = parse_idempotency_key("--idempotency-key", &args.idempotency_key)?;
    let kind = match args.kind {
        crate::cli::TreasuryKind::PlatformFees => WithdrawalKind::PlatformFees,
        crate::cli::TreasuryKind::CompensationReserve => WithdrawalKind::CompensationReserve,
    };
    println!(
        "withdraw {} {} cycles to {} (idempotency {})",
        args.kind.name(),
        args.amount,
        destination,
        to_hex(&idempotency_key)
    );
    if !args.yes {
        println!("plan only; re-run with --yes to execute");
        return Ok(());
    }
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let result = client
        .withdraw_fees(WireWithdrawArgs {
            idempotency_key,
            kind,
            destination,
            amount: args.amount,
        })
        .await?
        .map_err(withdraw_error)?;
    println!(
        "withdrawn amount={} replayed={}",
        result.amount, result.replayed
    );
    Ok(())
}

/// `treasury status`: print one exact-key withdrawal record.
async fn run_treasury_status(
    connection: &Connection,
    args: TreasuryStatusArgs,
) -> Result<(), OperatorError> {
    let idempotency_key = parse_idempotency_key("--idempotency-key", &args.idempotency_key)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    match client
        .get_withdrawal(idempotency_key)
        .await?
        .map_err(withdraw_error)?
    {
        None => println!("no withdrawal for {}", to_hex(&idempotency_key)),
        Some(record) => {
            println!("kind: {:?}", record.kind);
            println!("destination: {}", record.destination);
            println!("amount: {}", record.amount);
            match record.status {
                WithdrawalStatus::Pending => println!("status: pending"),
                WithdrawalStatus::Completed => println!("status: completed"),
                WithdrawalStatus::Failed(reason) => println!("status: failed: {reason}"),
            }
        }
    }
    Ok(())
}

fn grant_error(error: GrantCreditError) -> OperatorError {
    OperatorError::Message(format!("grant_credit rejected: {error:?}"))
}

fn reverse_error(error: ReverseCreditError) -> OperatorError {
    OperatorError::Message(format!("reverse_credit rejected: {error:?}"))
}

/// `billing grant`: preview unless `--yes`, then grant credit idempotently.
async fn run_billing_grant(
    connection: &Connection,
    args: BillingGrantArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let idempotency_key = parse_idempotency_key("--idempotency-key", &args.idempotency_key)?;
    println!(
        "grant {} cycles to {} (idempotency {})",
        args.amount,
        deployment,
        to_hex(&idempotency_key)
    );
    if !args.yes {
        println!("plan only; re-run with --yes to execute");
        return Ok(());
    }
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let result = client
        .grant_credit(GrantCreditArgs {
            deployment_id: deployment,
            amount: args.amount,
            idempotency_key,
        })
        .await?
        .map_err(grant_error)?;
    println!(
        "granted deployment={} amount={} credit={} replayed={}",
        result.deployment_id, result.amount, result.credit_cycles, result.replayed
    );
    Ok(())
}

/// `billing reverse`: preview unless `--yes`, then reverse an unused full grant.
async fn run_billing_reverse(
    connection: &Connection,
    args: BillingReverseArgs,
) -> Result<(), OperatorError> {
    let grant_authority = parse_principal("--grant-authority", &args.grant_authority)?;
    let idempotency_key = parse_idempotency_key("--idempotency-key", &args.idempotency_key)?;
    println!(
        "reverse grant {} key {}",
        grant_authority,
        to_hex(&idempotency_key)
    );
    if !args.yes {
        println!("plan only; re-run with --yes to execute");
        return Ok(());
    }
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let result = client
        .reverse_credit(ReverseCreditArgs {
            grant_authority,
            idempotency_key,
        })
        .await?
        .map_err(reverse_error)?;
    println!(
        "reversed deployment={} amount={} credit={} replayed={}",
        result.deployment_id, result.amount, result.credit_cycles, result.replayed
    );
    Ok(())
}

/// `deployment show`: print one deployment record for reconciliation.
/// `deployment reconcile`: read-only comparison of the recorded canisters with the
/// management canister's `canister_status`. Missing, stopped, or controller-mismatched
/// canisters make the command exit non-zero so a cron/runbook can alert on it.
async fn run_deployment_reconcile(
    connection: &Connection,
    args: DeploymentReconcileArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let record = client
        .deployment(deployment)
        .await?
        .map_err(|error| OperatorError::Message(format!("get_deployment rejected: {error:?}")))?;
    let canisters = client
        .deployment_canisters(deployment)
        .await?
        .map_err(|error| {
            OperatorError::Message(format!("get_deployment_canisters rejected: {error:?}"))
        })?;

    let mut expected = std::collections::BTreeSet::from([connection.provision()]);
    match record.controller_mode {
        crate::wire::ControllerMode::Managed => {
            let operator = record.operator.ok_or_else(|| {
                OperatorError::Message("managed deployment record has no operator".into())
            })?;
            expected.insert(operator);
        }
        crate::wire::ControllerMode::SelfCustody => {
            expected.insert(record.owner);
        }
    }

    println!("deployment: {deployment}");
    println!("recorded_canisters: {}", canisters.len());
    println!(
        "expected_controllers: {}",
        expected
            .iter()
            .map(|p| p.to_text())
            .collect::<Vec<_>>()
            .join(",")
    );

    let mut unhealthy = 0usize;
    let mut active_count = 0usize;
    let mut orphan_count = 0usize;
    for record in canisters {
        if !record.active {
            if !args.include_failed {
                continue;
            }
            match crate::bootstrap::read_canister_status(
                connection.network(),
                connection.identity(),
                record.canister,
            )
            .await
            {
                Ok(status) => {
                    orphan_count += 1;
                    println!(
                        "canister {}: failed_job_canister status={} orphan=true",
                        record.canister,
                        status.status.name()
                    );
                }
                Err(error) => {
                    println!(
                        "canister {}: failed_job_canister_missing ({error})",
                        record.canister
                    );
                }
            }
            continue;
        }
        active_count += 1;
        let canister = record.canister;
        match crate::bootstrap::read_canister_status(
            connection.network(),
            connection.identity(),
            canister,
        )
        .await
        {
            Ok(status) => {
                let actual: std::collections::BTreeSet<_> = status
                    .settings
                    .as_ref()
                    .and_then(|settings| settings.controllers.clone())
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                let running = status.status == crate::wire::ManagementStatusKind::Running;
                let controllers_match = actual == expected;
                if !running || !controllers_match {
                    unhealthy += 1;
                }
                println!(
                    "canister {canister}: status={} controllers_match={} running={}",
                    status.status.name(),
                    controllers_match,
                    running
                );
            }
            Err(error) => {
                unhealthy += 1;
                println!("canister {canister}: status_unavailable ({error})");
            }
        }
    }

    if unhealthy == 0 {
        println!(
            "reconcile: ok ({active_count} active canisters, {orphan_count} failed-job canisters still present)"
        );
        Ok(())
    } else {
        Err(OperatorError::Message(format!(
            "reconcile found {unhealthy} unhealthy canister(s)"
        )))
    }
}

/// `deployment repair`: delete canisters recorded only by Failed jobs. Preview by default;
/// `--yes` executes. The Provision endpoint re-checks that a canister is still inactive.
async fn run_deployment_repair(
    connection: &Connection,
    args: DeploymentRepairArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    if !args.yes {
        let canisters = client
            .deployment_canisters(deployment)
            .await?
            .map_err(|error| {
                OperatorError::Message(format!("get_deployment_canisters rejected: {error:?}"))
            })?;
        let failed: Vec<_> = canisters.iter().filter(|c| !c.active).collect();
        println!("repair plan: {} failed-job canister(s)", failed.len());
        for canister in failed {
            println!("would delete {}", canister.canister);
        }
        println!("re-run with --yes to delete");
        return Ok(());
    }
    let outcomes = client
        .repair_failed_deployment_canisters(deployment)
        .await?
        .map_err(|error| {
            OperatorError::Message(format!(
                "repair_failed_deployment_canisters rejected: {error:?}"
            ))
        })?;
    let mut failures = 0usize;
    for outcome in outcomes {
        match outcome {
            crate::wire::FailedCanisterRepair::Deleted => println!("deleted"),
            crate::wire::FailedCanisterRepair::AlreadyAbsent => println!("already absent"),
            crate::wire::FailedCanisterRepair::SkippedActive => println!("skipped: active again"),
            crate::wire::FailedCanisterRepair::Failed { reason } => {
                failures += 1;
                println!("failed: {reason}");
            }
        }
    }
    if failures == 0 {
        Ok(())
    } else {
        Err(OperatorError::Message(format!(
            "repair failed for {failures} canister(s)"
        )))
    }
}

async fn run_deployment_jobs(
    connection: &Connection,
    args: DeploymentJobsArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let jobs = client
        .deployment_jobs(deployment)
        .await?
        .map_err(|error| OperatorError::Message(format!("deployment_jobs rejected: {error:?}")))?;
    let requested = args
        .request_id
        .as_deref()
        .map(|value| parse_idempotency_key("--request-id", value))
        .transpose()?;
    let jobs: Vec<_> = jobs
        .into_iter()
        .filter(|job| requested.is_none_or(|request_id| job.request_id == request_id))
        .collect();
    if jobs.is_empty() {
        match requested {
            Some(request_id) => println!("no job {} for {deployment}", to_hex(&request_id)),
            None => println!("no jobs for {deployment}"),
        }
        return Ok(());
    }
    println!("deployment: {deployment}");
    println!("jobs: {}", jobs.len());
    for job in jobs {
        println!(
            "request_id={} state={} effects={} resources={} graph={} reserved_graph={} created_at_ns={} last_transition_ns={}",
            to_hex(&job.request_id),
            job.state_name,
            job.completed_effect_count,
            job.resource_count,
            job.graph_name,
            job.reserved_graph_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "none".into()),
            job.created_at_ns,
            job.last_transition_ns,
        );
    }
    Ok(())
}

async fn run_deployment_job(
    connection: &Connection,
    args: DeploymentJobArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let request_id = parse_idempotency_key("--request-id", &args.request_id)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let job = client
        .deployment_job(deployment, request_id)
        .await?
        .map_err(|error| OperatorError::Message(format!("deployment_job rejected: {error:?}")))?;
    let Some(job) = job else {
        return Err(OperatorError::Message(format!(
            "no job {} for {deployment}",
            to_hex(&request_id)
        )));
    };
    println!("deployment: {deployment}");
    println!("request_id: {}", to_hex(&job.request_id));
    println!("state: {}", job.state_name);
    println!("graph: {}", job.graph_name);
    println!(
        "reserved_graph: {}",
        job.reserved_graph_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".into())
    );
    println!("active_resource_index: {}", job.active_resource_index);
    println!("completed_effect_count: {}", job.completed_effect_count);
    println!("is_authorized_caller: {}", job.is_authorized_caller);
    println!("resources: {}", job.resources.len());
    for resource in job.resources {
        println!(
            "resource={:?} canister={} artifact_hash={}",
            resource.logical_resource,
            resource
                .canister_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "none".into()),
            resource
                .artifact_hash
                .map(|hash| to_hex(&hash))
                .unwrap_or_else(|| "none".into()),
        );
    }
    Ok(())
}

async fn run_deployment_set_owner(
    connection: &Connection,
    args: DeploymentSetOwnerArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let owner = parse_principal("--owner", &args.owner)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let current = client
        .deployment(deployment)
        .await?
        .map_err(|error| OperatorError::Message(format!("get_deployment rejected: {error:?}")))?;
    println!("deployment: {deployment}");
    println!("controller_mode: {:?}", current.controller_mode);
    println!("current_owner: {}", current.owner);
    println!("new_owner: {owner}");
    if !args.yes {
        println!("re-run with --yes to set the owner");
        return Ok(());
    }
    let record = client
        .set_deployment_owner(deployment, owner)
        .await?
        .map_err(|error| {
            OperatorError::Message(format!("set_deployment_owner rejected: {error:?}"))
        })?;
    println!("owner: {}", record.owner);
    println!("controller_mode: {:?}", record.controller_mode);
    Ok(())
}

async fn run_deployment_take_ownership(
    connection: &Connection,
    args: DeploymentTakeOwnershipArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let current = client
        .deployment(deployment)
        .await?
        .map_err(|error| OperatorError::Message(format!("get_deployment rejected: {error:?}")))?;
    println!("deployment: {deployment}");
    println!("controller_mode: {:?}", current.controller_mode);
    println!("owner: {}", current.owner);
    println!("operator: {:?}", current.operator);
    if !args.yes {
        println!("re-run with --yes to move controllers to the owner");
        return Ok(());
    }
    let record = client
        .take_ownership(deployment)
        .await?
        .map_err(|error| OperatorError::Message(format!("take_ownership rejected: {error:?}")))?;
    println!("controller_mode: {:?}", record.controller_mode);
    println!("owner: {}", record.owner);
    println!("operator: {:?}", record.operator);
    Ok(())
}

async fn run_deployment_transfer_self_custody(
    connection: &Connection,
    args: DeploymentTransferSelfCustodyArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let owner = parse_principal("--owner", &args.owner)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let current = client
        .deployment(deployment)
        .await?
        .map_err(|error| OperatorError::Message(format!("get_deployment rejected: {error:?}")))?;
    println!("deployment: {deployment}");
    println!("controller_mode: {:?}", current.controller_mode);
    println!("current_owner: {}", current.owner);
    println!("new_owner: {owner}");
    if !args.yes {
        println!("re-run with --yes to transfer SelfCustody ownership");
        return Ok(());
    }
    let record = client
        .transfer_self_custody(deployment, owner)
        .await?
        .map_err(|error| {
            OperatorError::Message(format!("transfer_self_custody rejected: {error:?}"))
        })?;
    println!("owner: {}", record.owner);
    println!("controller_mode: {:?}", record.controller_mode);
    Ok(())
}

async fn run_billing_grants(
    connection: &Connection,
    args: BillingGrantsArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let grants = client
        .billing_grants(deployment)
        .await?
        .map_err(|error| OperatorError::Message(format!("billing_grants rejected: {error:?}")))?;
    println!("deployment: {deployment}");
    println!("grants: {}", grants.len());
    for grant in grants {
        print_billing_grant(&grant);
    }
    Ok(())
}

async fn run_billing_grant_status(
    connection: &Connection,
    args: BillingGrantStatusArgs,
) -> Result<(), OperatorError> {
    let authority = parse_principal("--grant-authority", &args.grant_authority)?;
    let key = parse_idempotency_key("--idempotency-key", &args.idempotency_key)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let grant = client
        .billing_grant_status(authority, key)
        .await?
        .map_err(|error| {
            OperatorError::Message(format!("billing_grant_status rejected: {error:?}"))
        })?;
    let Some(grant) = grant else {
        return Err(OperatorError::Message(format!(
            "no grant {} for {authority}",
            to_hex(&key)
        )));
    };
    print_billing_grant(&grant);
    Ok(())
}

fn print_billing_grant(grant: &BillingGrantView) {
    println!(
        "authority={} key={} deployment={} amount={} credited_at_ns={} reversed_at_ns={}",
        grant.authority,
        to_hex(&grant.idempotency_key),
        grant.deployment_id,
        grant.amount,
        grant.credited_at_ns,
        grant
            .reversed_at_ns
            .map(|ns| ns.to_string())
            .unwrap_or_else(|| "none".into()),
    );
}

async fn run_deployment_termination(
    connection: &Connection,
    args: DeploymentTerminationArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let record = client
        .deployment_termination(deployment)
        .await?
        .map_err(|error| {
            OperatorError::Message(format!("deployment_termination rejected: {error:?}"))
        })?;
    match record {
        Some(record) => print_termination(&record),
        None => println!("deployment {deployment}: not suspended"),
    }
    Ok(())
}

async fn run_deployment_suspend(
    connection: &Connection,
    args: DeploymentSuspendArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    if !args.yes {
        let record = client
            .deployment_termination(deployment)
            .await?
            .map_err(|error| {
                OperatorError::Message(format!("deployment_termination rejected: {error:?}"))
            })?;
        match record {
            Some(record) => print_termination(&record),
            None => println!("deployment {deployment}: active; re-run with --yes to suspend"),
        }
        return Ok(());
    }
    let record = client
        .request_deployment_termination(deployment, args.reason)
        .await?
        .map_err(|error| OperatorError::Message(format!("suspend rejected: {error:?}")))?;
    print_termination(&record);
    Ok(())
}

async fn run_deployment_resume(
    connection: &Connection,
    args: DeploymentResumeArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    if !args.yes {
        let record = client
            .deployment_termination(deployment)
            .await?
            .map_err(|error| {
                OperatorError::Message(format!("deployment_termination rejected: {error:?}"))
            })?;
        match record {
            Some(record) => print_termination(&record),
            None => println!("deployment {deployment}: active; nothing to resume"),
        }
        return Ok(());
    }
    let record = client
        .cancel_deployment_termination(deployment)
        .await?
        .map_err(|error| OperatorError::Message(format!("resume rejected: {error:?}")))?;
    print_termination(&record);
    Ok(())
}

fn print_termination(record: &ProvisionDeploymentTermination) {
    println!("deployment: {}", record.deployment_id);
    println!("state: {:?}", record.state);
    println!("reason: {}", record.reason);
    println!("initiator: {}", record.initiator);
    println!("requested_at_ns: {}", record.requested_at_ns);
    println!("grace_until_ns: {}", record.grace_until_ns);
    println!(
        "cancelled_at_ns: {}",
        record
            .cancelled_at_ns
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".into())
    );
    println!("canisters: {}", record.canisters.len());
    println!("pending_canisters: {}", record.pending_canisters.len());
}

async fn run_deployment_show(
    connection: &Connection,
    args: DeploymentShowArgs,
) -> Result<(), OperatorError> {
    let deployment = parse_principal("--deployment", &args.deployment)?;
    let ingress = connection.connect_ingress().await?;
    let client = ProvisionClient::new(&ingress, connection.provision());
    let record = client
        .deployment(deployment)
        .await?
        .map_err(|error| OperatorError::Message(format!("get_deployment rejected: {error:?}")))?;
    println!("deployment: {deployment}");
    println!("owner: {}", record.owner);
    println!("controller_mode: {:?}", record.controller_mode);
    println!(
        "operator: {}",
        record
            .operator
            .map(|p| p.to_text())
            .unwrap_or_else(|| "none".into())
    );
    println!("credit_cycles: {}", record.credit_cycles);
    Ok(())
}
