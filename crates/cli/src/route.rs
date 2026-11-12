//! Graph routing profile commands (ADR 0115): read and set the per-graph abstain threshold and
//! policy version. Authorization is enforced by the Router (controller or graph tenant).

use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde::Serialize;
use thiserror::Error;

use crate::ask;

/// `gleaph route` command group.
#[derive(Debug, Args)]
pub struct RouteArgs {
    /// Routing subcommand.
    #[command(subcommand)]
    pub command: RouteCommand,
}

/// Routing subcommands.
#[derive(Debug, Subcommand)]
pub enum RouteCommand {
    /// Read or set one graph's general-routing profile.
    Profile(ProfileArgs),
}

/// Arguments for `gleaph route profile`.
#[derive(Debug, Args)]
pub struct ProfileArgs {
    /// Router canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: String,
    /// Inference-enabled graph name.
    #[arg(long, value_name = "NAME")]
    pub graph: String,
    /// Network name (`local`) or an HTTP(S) endpoint.
    #[arg(short, long, default_value = "local", value_name = "NETWORK")]
    pub network: String,
    /// PEM identity; anonymous when omitted.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before the update on a custom endpoint.
    #[arg(long)]
    pub fetch_root_key: bool,
    /// Set the abstain threshold in basis points (0..=10000); omit to read only.
    #[arg(long, value_name = "BPS")]
    pub set_threshold_bps: Option<u32>,
    /// Policy version written with `--set-threshold-bps`.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub policy_version: u32,
    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Routing profile as returned by the Router.
#[derive(Debug, Serialize)]
pub struct ProfileReport {
    /// Graph name.
    pub graph: String,
    /// Current abstain threshold in basis points.
    pub abstain_threshold_bps: u32,
    /// Current policy version.
    pub policy_version: u32,
    /// Whether this invocation wrote the profile.
    pub updated: bool,
}

/// Errors raised by `gleaph route`.
#[derive(Debug, Error)]
pub enum RouteError {
    /// Connecting to the Router or the Router call failed.
    #[error("router: {0}")]
    Router(String),
    /// The profile pair could not be rendered.
    #[error("json: {0}")]
    Json(String),
}

impl From<ask::AskError> for RouteError {
    fn from(error: ask::AskError) -> Self {
        Self::Router(error.to_string())
    }
}

/// Execute `gleaph route`.
pub fn execute(args: RouteArgs) -> Result<(), RouteError> {
    match args.command {
        RouteCommand::Profile(args) => profile(args),
    }
}

fn profile(args: ProfileArgs) -> Result<(), RouteError> {
    if let Some(bps) = args.set_threshold_bps
        && bps > 10_000
    {
        return Err(RouteError::Router(format!(
            "abstain threshold must be in 0..=10000 basis points, got {bps}"
        )));
    }
    let client = ask::connect_routing_client(
        &args.canister,
        &args.graph,
        &args.network,
        args.identity.as_deref(),
        args.fetch_root_key,
    )?;
    let updated = args.set_threshold_bps.is_some();
    if let Some(bps) = args.set_threshold_bps {
        client
            .set_routing_profile(&args.graph, bps, args.policy_version)
            .map_err(|error| RouteError::Router(error.to_string()))?;
    }
    let (abstain_threshold_bps, policy_version) = client
        .routing_profile(&args.graph)
        .map_err(|error| RouteError::Router(error.to_string()))?;
    let report = ProfileReport {
        graph: args.graph,
        abstain_threshold_bps,
        policy_version,
        updated,
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| RouteError::Json(error.to_string()))?
        );
    } else {
        println!("graph: {}", report.graph);
        println!("abstain_threshold_bps: {}", report.abstain_threshold_bps);
        println!("policy_version: {}", report.policy_version);
        if report.updated {
            println!("updated: true");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_range_threshold_before_connecting() {
        let error = execute(RouteArgs {
            command: RouteCommand::Profile(ProfileArgs {
                canister: "aaaaa-aa".to_owned(),
                graph: "movies".to_owned(),
                network: "local".to_owned(),
                identity: None,
                fetch_root_key: false,
                set_threshold_bps: Some(10_001),
                policy_version: 1,
                json: false,
            }),
        })
        .expect_err("threshold is out of range");
        assert!(matches!(error, RouteError::Router(message) if message.contains("0..=10000")));
    }
}
