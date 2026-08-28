//! `gleaph vector activate` / `deactivate` — the global vector-dispatch kill-switch.
//!
//! The flag is fleet-level and defaults to ENABLED (dispatch readiness is the per-index
//! lifecycle's job — target + shard attach): `deactivate` is the incident-response circuit
//! breaker, not a required setup step. The flag is stable-memory state with an operator-set
//! value preserved across upgrades, and it stays OUT of the migration lane (migrations are
//! schema; a kill-switch is runtime ops state).

use std::path::{Path, PathBuf};

use crate::config::LoadedConfig;

/// CLI arguments for `gleaph vector activate` / `deactivate`.
#[derive(Debug, clap::Args)]
pub struct VectorActivateArgs {
    /// Router canister principal (required unless supplied by GLEAPH_CANISTER or `gleaph.toml`).
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: Option<String>,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(short = 'n', long, value_name = "NETWORK")]
    pub network: Option<String>,
    /// PEM file containing a Secp256k1 identity.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub fetch_root_key: Option<bool>,
}

/// `gleaph vector` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum VectorCommand {
    /// Enable global vector dispatch (requires MANAGE_FEDERATION).
    Activate(VectorActivateArgs),
    /// Disable global vector dispatch (fail-closed across all graphs).
    Deactivate(VectorActivateArgs),
}

#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    #[error("transport: {0}")]
    Remote(String),
    #[error("router rejected the activation: {0:?}")]
    Router(String),
}

/// Flip the Router's global vector-dispatch flag as the session identity. The Router
/// gates the call on `MANAGE_FEDERATION` (the bootstrap principal holds it), so the
/// error surfaces verbatim when the caller is not an operator.
pub fn set_dispatch_enabled(
    enabled: bool,
    canister: &str,
    network: &str,
    identity: Option<&Path>,
    fetch_root_key: bool,
    project_root: Option<&Path>,
    _loaded: Option<&LoadedConfig>,
) -> Result<(), VectorError> {
    let remote = crate::remote::RemoteTransport::connect(
        canister,
        network,
        identity,
        fetch_root_key,
        project_root,
    )
    .map_err(VectorError::Remote)?;
    let decoded: Result<(), gleaph_graph_kernel::federation::RouterError> = remote
        .update("set_vector_dispatch_enabled", &enabled)
        .map_err(VectorError::Remote)?;
    decoded.map_err(|error| VectorError::Router(format!("{error:?}")))
}
