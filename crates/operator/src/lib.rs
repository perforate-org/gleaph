//! Gleaph platform-operator tool (ADR 0087).
//!
//! `gleaph-operator` is the persona boundary for platform operations: artifact
//! publish-metadata / upload-chunk / get-status, release publish / activate / get-active,
//! release-install to an explicit target canister, deployment-binding install, and
//! audit-history readback (slices 2–3), plus the bootstrap tier — Account/Provision
//! self-deploy/upgrade through the IC management canister, delivered before first
//! production operation per ADR 0087 §Explicitly deferred. All protocol logic (chunk
//! splitting, hashing, ordering, idempotent resume) lives in the shared [`gleaph_artifact_api`]
//! library; this crate owns the command surface, the IC transport, and the wire mirrors for
//! the operations outside the ingestion pipeline (`release_install`,
//! `upsert_deployment_grant`, `artifact_audit_history`).
//!
//! Module map:
//! - [`cli`]: clap definitions (pure parsing, unit-tested offline).
//! - [`commands`]: command → logic mapping and output printing.
//! - [`transport`]: the generic any-canister/any-method IC ingress layer plus the typed
//!   Provision client implementing [`gleaph_artifact_api::ArtifactTransport`].
//! - [`bootstrap`]: bootstrap-tier Account/Provision self-deploy/upgrade through the IC
//!   management canister (ADR 0087 §Explicitly deferred), reusing [`transport`].
//! - [`net`]: network/endpoint resolution and PEM identity handling (dev-CLI conventions).
//! - [`wire`]: mirrored candid types for install/binding/audit operations.
//! - [`encoding`]: local textual forms — kind names, SHA-256 hex, identity labels.
//! - [`error`]: operator-facing error type with human-readable rendering for every server
//!   rejection variant.
//! - [`manifest`]: release-publish JSON manifest loading.

#![warn(missing_docs)]

pub mod bootstrap;
pub mod cli;
pub mod commands;
pub mod encoding;
pub mod error;
pub mod manifest;
pub mod net;
pub mod transport;
pub mod wire;

use crate::cli::{Cli, Command};
use crate::error::OperatorError;

/// Run one parsed command line to completion. The binary shell only adds the async runtime
/// and the process exit code around this entry point.
pub async fn run(cli: Cli) -> Result<(), OperatorError> {
    // Bootstrap-tier commands target the management canister directly; they neither need
    // nor accept --provision, so connection resolution is told to skip it.
    let requires_provision = !matches!(cli.command, Command::Bootstrap(_));
    let connection = commands::Connection::resolve(
        cli.provision.as_deref(),
        &cli.network,
        cli.identity.as_deref(),
        requires_provision,
    )?;
    commands::execute(cli.command, &connection).await
}
