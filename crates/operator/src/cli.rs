//! Clap command definitions for `gleaph-operator`.
//!
//! Pure parsing only: every struct here maps 1:1 onto a [`crate::commands`] entry point and
//! is unit-tested for that mapping. Connection flags are global so they can be placed before
//! or after the subcommand (`gleaph-operator -n local artifact ingest …`).

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Top-level operator command line.
#[derive(Debug, Parser)]
#[command(
    name = "gleaph-operator",
    about = "Gleaph platform-operator tool: Provision artifact catalog operations (ADR 0087)"
)]
pub struct Cli {
    /// Provision canister principal.
    #[arg(long, value_name = "PRINCIPAL", global = true)]
    pub provision: Option<String>,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(
        short = 'n',
        long,
        value_name = "NETWORK",
        default_value = "local",
        global = true
    )]
    pub network: String,
    /// PEM file containing the governance Secp256k1 identity (anonymous when omitted).
    #[arg(long, value_name = "PATH", global = true)]
    pub identity: Option<PathBuf>,
    /// Operation to perform.
    #[command(subcommand)]
    pub command: Command,
}

/// All operations surface through five groups mirroring ADR 0087 §Surfaces.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Artifact catalog: publish metadata, upload chunks idempotently, poll status.
    #[command(subcommand)]
    Artifact(ArtifactCommand),
    /// Release manifests: publish, activate atomically, or read the active release.
    #[command(subcommand)]
    Release(ReleaseCommand),
    /// Canister lifecycle against active-release artifacts.
    #[command(subcommand)]
    Canister(CanisterCommand),
    /// Deployment grant commands (authorize an issuer to request issuance).
    #[command(subcommand)]
    Grant(GrantCommand),
    /// Read the durable artifact/release audit history.
    #[command(subcommand)]
    Audit(AuditCommand),
    /// Bootstrap tier: Account/Provision self-deploy/upgrade through the IC management
    /// canister (ADR 0087 §Explicitly deferred, delivered before first mainnet operation).
    #[command(subcommand)]
    Bootstrap(BootstrapCommand),
}

/// Which bootstrap-tier canister a command operates on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BootstrapKind {
    /// The Account canister (init takes no arguments).
    Account,
    /// The Provision canister (init takes `ProvisionInitArgs`).
    Provision,
}

impl BootstrapKind {
    /// Lowercase name used on the command line and in output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Provision => "provision",
        }
    }
}

/// Bootstrap-tier operations (management-canister destination `aaaaa-aa`).
#[derive(Debug, Subcommand)]
pub enum BootstrapCommand {
    /// Create a fresh canister as the caller's principal, upload wasm chunks into its own
    /// chunk store, then install them (`install_chunked_code` mode=install).
    Deploy(BootstrapDeployArgs),
    /// Stop an existing canister, upload wasm chunks, upgrade in place, restart.
    Upgrade(BootstrapUpgradeArgs),
    /// Print `canister_status`: state, cycles, module hash.
    Status(BootstrapStatusArgs),
}

/// Init-argument flags shared by `bootstrap deploy` / `bootstrap upgrade`.
///
/// Exactly one form may be given: `--init-args <JSON>` builds the candid bytes from a
/// typed mirror (Provision only), `--init-args-hex <HEX>` is the universal escape hatch
/// carrying pre-encoded candid bytes verbatim.
#[derive(Debug, clap::Args)]
pub struct BootstrapInitArgs {
    /// Candid-encoded init argument as hex (escape hatch; forwarded verbatim).
    #[arg(long, value_name = "HEX", group = "init-args")]
    pub init_args_hex: Option<String>,
    /// Init argument as JSON, built through the typed mirror (`provision` only;
    /// schema: crate::bootstrap::ProvisionInitArgsInput).
    #[arg(long, value_name = "JSON", group = "init-args")]
    pub init_args: Option<String>,
}

/// Arguments of `bootstrap deploy`.
#[derive(Debug, clap::Args)]
pub struct BootstrapDeployArgs {
    /// Which platform canister to deploy.
    #[arg(value_name = "TARGET")]
    pub kind: BootstrapKind,
    /// Path to the wasm file to install.
    #[arg(long, value_name = "PATH")]
    pub wasm: PathBuf,
    /// Init-argument flags (`--init-args` / `--init-args-hex`, mutually exclusive).
    #[command(flatten)]
    pub init_args: BootstrapInitArgs,
    /// Cycles attached to `create_canister`. Defaults to the same amount Provision's
    /// issuance attaches to freshly created canisters (INITIAL_CANISTER_CYCLES,
    /// crates/provision/src/canister/mod.rs).
    #[arg(long, value_name = "CYCLES")]
    pub cycles: Option<u128>,
    /// Execute; without this flag only the plan is printed.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments of `bootstrap upgrade`.
#[derive(Debug, clap::Args)]
pub struct BootstrapUpgradeArgs {
    /// Which platform canister to upgrade.
    #[arg(value_name = "TARGET")]
    pub kind: BootstrapKind,
    /// Existing target canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    pub target: String,
    /// Path to the wasm file to install.
    #[arg(long, value_name = "PATH")]
    pub wasm: PathBuf,
    /// Init-argument flags (`--init-args` / `--init-args-hex`, mutually exclusive).
    #[command(flatten)]
    pub init_args: BootstrapInitArgs,
    /// Execute; without this flag only the plan is printed.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments of `bootstrap status`.
#[derive(Debug, clap::Args)]
pub struct BootstrapStatusArgs {
    /// Target canister principal (must have the caller as controller).
    #[arg(long, value_name = "PRINCIPAL")]
    pub target: String,
}

/// Canister lifecycle commands.
#[derive(Debug, Subcommand)]
pub enum CanisterCommand {
    /// Install the active release's artifact into an explicit target canister.
    Install(CanisterInstallArgs),
}

/// Deployment grant commands.
#[derive(Debug, Subcommand)]
pub enum GrantCommand {
    /// Authorize an issuer to request issuance for its own deployment (governance only).
    Upsert(GrantUpsertArgs),
}

/// Artifact catalog commands.
#[derive(Debug, Subcommand)]
pub enum ArtifactCommand {
    /// Plan a wasm file locally, then drive the idempotent ingestion pipeline.
    Ingest(ArtifactIngestArgs),
    /// Query upload status by the identifying triple (kind + version + sha256).
    Status(ArtifactStatusArgs),
}

/// Arguments of `artifact ingest`.
#[derive(Debug, clap::Args)]
pub struct ArtifactIngestArgs {
    /// Path to the wasm file to ingest.
    pub wasm: PathBuf,
    /// Target canister kind (Router | Graph | PropertyIndex | VectorCanister | TextCanister).
    #[arg(long, value_name = "KIND")]
    pub kind: String,
    /// Semantic version to publish under.
    #[arg(long, value_name = "SEMVER")]
    pub version: String,
}

/// Arguments of `artifact status`.
#[derive(Debug, clap::Args)]
pub struct ArtifactStatusArgs {
    /// Target canister kind.
    #[arg(long, value_name = "KIND")]
    pub kind: String,
    /// Semantic version.
    #[arg(long, value_name = "SEMVER")]
    pub version: String,
    /// Full SHA-256 of the artifact bytes (64 hex characters).
    #[arg(long = "sha256", value_name = "HEX")]
    pub sha256: String,
}

/// Release commands.
#[derive(Debug, Subcommand)]
pub enum ReleaseCommand {
    /// Publish a release manifest declaring exactly one verified artifact per kind.
    Publish(ReleasePublishCliArgs),
    /// Activate a published release (the atomic, release-scoped trust act).
    Activate(ReleaseActivateCliArgs),
    /// Print the currently active release, if any.
    GetActive,
}

/// Arguments of `release publish`.
#[derive(Debug, clap::Args)]
pub struct ReleasePublishCliArgs {
    /// JSON manifest path; see crate::manifest for the schema.
    #[arg(long, value_name = "PATH")]
    pub manifest: PathBuf,
}

/// Arguments of `release activate`.
#[derive(Debug, clap::Args)]
pub struct ReleaseActivateCliArgs {
    /// Release identifier previously published.
    #[arg(long, value_name = "ID")]
    pub release_id: String,
}

/// Arguments of `canister install`.
#[derive(Debug, clap::Args)]
pub struct CanisterInstallArgs {
    /// Explicit target canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    pub target: String,
    /// Which active-release artifact to install.
    #[arg(long, value_name = "KIND")]
    pub kind: String,
    /// Candid-encoded init argument as hex (empty when omitted).
    #[arg(long, value_name = "HEX", default_value = "")]
    pub install_args_hex: String,
    /// Registry version recorded on the install call.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub registry_version: u64,
}

/// Arguments of `grant upsert`.
#[derive(Debug, clap::Args)]
pub struct GrantUpsertArgs {
    /// The principal authorized to request issuance. The deployment is the issuer itself:
    /// `deployment_id = issuer = caller`.
    #[arg(value_name = "ISSUER")]
    pub issuer: String,
}

/// Audit commands.
#[derive(Debug, Subcommand)]
pub enum AuditCommand {
    /// Print every audit row Provision returns for the caller.
    History,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("gleaph-operator").chain(args.iter().copied()))
            .expect("parse")
    }

    fn parse_err(args: &[&str]) -> String {
        Cli::try_parse_from(std::iter::once("gleaph-operator").chain(args.iter().copied()))
            .expect_err("must fail")
            .to_string()
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn ingest_maps_positional_wasm_and_kind_version_flags() {
        let cli = parse(&[
            "artifact",
            "ingest",
            "router.wasm",
            "--kind",
            "Graph",
            "--version",
            "1.2.3",
        ]);
        assert_eq!(cli.network, "local", "--network defaults to local");
        assert!(cli.provision.is_none());
        assert!(cli.identity.is_none());
        match cli.command {
            Command::Artifact(ArtifactCommand::Ingest(args)) => {
                assert_eq!(args.wasm, PathBuf::from("router.wasm"));
                assert_eq!(args.kind, "Graph");
                assert_eq!(args.version, "1.2.3");
            }
            other => panic!("wrong command mapping: {other:?}"),
        }
    }

    #[test]
    fn global_connection_flags_work_before_and_after_the_subcommand() {
        let after = parse(&[
            "artifact",
            "status",
            "--kind",
            "Router",
            "--version",
            "0.1.0",
            "--sha256",
            &"a".repeat(64),
            "--provision",
            "r7inp-6aaaa-aaaaa-aaabq-cai",
            "-n",
            "ic",
            "--identity",
            "/tmp/gov.pem",
        ]);
        assert_eq!(
            after.provision.as_deref(),
            Some("r7inp-6aaaa-aaaaa-aaabq-cai")
        );
        assert_eq!(after.network, "ic");
        assert_eq!(
            after.identity.as_deref(),
            Some(std::path::Path::new("/tmp/gov.pem"))
        );

        // Same flags placed first must land in the same fields.
        let before = parse(&[
            "--provision",
            "r7inp-6aaaa-aaaaa-aaabq-cai",
            "artifact",
            "status",
            "--kind",
            "Router",
            "--version",
            "0.1.0",
            "--sha256",
            &"a".repeat(64),
        ]);
        assert_eq!(before.provision, after.provision);
        match before.command {
            Command::Artifact(ArtifactCommand::Status(args)) => {
                assert_eq!(args.kind, "Router");
                assert_eq!(args.version, "0.1.0");
                assert_eq!(args.sha256, "a".repeat(64));
            }
            other => panic!("wrong command mapping: {other:?}"),
        }
    }

    #[test]
    fn release_commands_map_manifest_and_release_id() {
        let cli = parse(&["release", "publish", "--manifest", "rel.json"]);
        match cli.command {
            Command::Release(ReleaseCommand::Publish(args)) => {
                assert_eq!(args.manifest, PathBuf::from("rel.json"));
            }
            other => panic!("wrong command mapping: {other:?}"),
        }

        let cli = parse(&["release", "activate", "--release-id", "rel-9"]);
        match cli.command {
            Command::Release(ReleaseCommand::Activate(args)) => {
                assert_eq!(args.release_id, "rel-9");
            }
            other => panic!("wrong command mapping: {other:?}"),
        }

        assert!(matches!(
            parse(&["release", "get-active"]).command,
            Command::Release(ReleaseCommand::GetActive)
        ));
    }

    #[test]
    fn canister_install_requires_target_kind_and_defaults_optional_fields() {
        let cli = parse(&[
            "canister",
            "install",
            "--target",
            "r7inp-6aaaa-aaaaa-aaabq-cai",
            "--kind",
            "TextCanister",
        ]);
        match cli.command {
            Command::Canister(CanisterCommand::Install(args)) => {
                assert_eq!(args.target, "r7inp-6aaaa-aaaaa-aaabq-cai");
                assert_eq!(args.kind, "TextCanister");
                assert_eq!(args.install_args_hex, "");
                assert_eq!(args.registry_version, 0);
            }
            other => panic!("wrong command mapping: {other:?}"),
        }

        let error = parse_err(&["canister", "install", "--kind", "Router"]);
        assert!(error.contains("--target"), "got: {error}");
    }
    #[test]
    fn grant_upsert_maps_issuer() {
        let cli = parse(&[
            "grant",
            "upsert",
            "r7inp-6aaaa-aaaaa-aaabq-cai",
        ]);
        match cli.command {
            Command::Grant(GrantCommand::Upsert(args)) => {
                assert_eq!(args.issuer, "r7inp-6aaaa-aaaaa-aaabq-cai");
            }
            other => panic!("wrong command mapping: {other:?}"),
        }
    }

    #[test]
    fn audit_history_parses() {
        assert!(matches!(
            parse(&["audit", "history"]).command,
            Command::Audit(AuditCommand::History)
        ));
    }

    #[test]
    fn bootstrap_deploy_maps_kind_wasm_and_defaults() {
        let cli = parse(&[
            "bootstrap",
            "deploy",
            "provision",
            "--wasm",
            "provision.wasm",
            "--init-args",
            r#"{"governance_principal":"renrz-6aaaa-aaaaa-aaabq-cai"}"#,
        ]);
        match cli.command {
            Command::Bootstrap(BootstrapCommand::Deploy(args)) => {
                assert_eq!(args.kind, BootstrapKind::Provision);
                assert_eq!(args.wasm, PathBuf::from("provision.wasm"));
                assert_eq!(
                    args.init_args.init_args.as_deref(),
                    Some(r#"{"governance_principal":"renrz-6aaaa-aaaaa-aaabq-cai"}"#)
                );
                assert!(args.init_args.init_args_hex.is_none());
                assert_eq!(args.cycles, None);
                assert!(!args.yes, "--yes must default to plan-only");
            }
            other => panic!("wrong command mapping: {other:?}"),
        }

        // Hex form + explicit cycles + confirm.
        let cli = parse(&[
            "bootstrap",
            "deploy",
            "account",
            "--wasm",
            "account.wasm",
            "--init-args-hex",
            "00ff",
            "--cycles",
            "2000000000000",
            "--yes",
        ]);
        match cli.command {
            Command::Bootstrap(BootstrapCommand::Deploy(args)) => {
                assert_eq!(args.kind, BootstrapKind::Account);
                assert_eq!(args.init_args.init_args_hex.as_deref(), Some("00ff"));
                assert!(args.init_args.init_args.is_none());
                assert_eq!(args.cycles, Some(2_000_000_000_000));
                assert!(args.yes);
            }
            other => panic!("wrong command mapping: {other:?}"),
        }
    }

    #[test]
    fn bootstrap_init_arg_forms_are_mutually_exclusive() {
        let error = parse_err(&[
            "bootstrap",
            "deploy",
            "provision",
            "--wasm",
            "p.wasm",
            "--init-args",
            "{}",
            "--init-args-hex",
            "00",
        ]);
        assert!(
            error.contains("cannot be used with"),
            "expected clap conflict, got: {error}"
        );
    }

    #[test]
    fn bootstrap_upgrade_requires_target_and_maps_all_flags() {
        let error = parse_err(&["bootstrap", "upgrade", "account", "--wasm", "a.wasm"]);
        assert!(error.contains("--target"), "got: {error}");

        let cli = parse(&[
            "bootstrap",
            "upgrade",
            "provision",
            "--target",
            "r7inp-6aaaa-aaaaa-aaabq-cai",
            "--wasm",
            "provision.wasm",
            "--yes",
        ]);
        match cli.command {
            Command::Bootstrap(BootstrapCommand::Upgrade(args)) => {
                assert_eq!(args.kind, BootstrapKind::Provision);
                assert_eq!(args.target, "r7inp-6aaaa-aaaaa-aaabq-cai");
                assert_eq!(args.wasm, PathBuf::from("provision.wasm"));
                assert!(args.yes);
            }
            other => panic!("wrong command mapping: {other:?}"),
        }
    }

    #[test]
    fn bootstrap_status_maps_target_principal() {
        let cli = parse(&[
            "bootstrap",
            "status",
            "--target",
            "r7inp-6aaaa-aaaaa-aaabq-cai",
        ]);
        match cli.command {
            Command::Bootstrap(BootstrapCommand::Status(args)) => {
                assert_eq!(args.target, "r7inp-6aaaa-aaaaa-aaabq-cai");
            }
            other => panic!("wrong command mapping: {other:?}"),
        }
    }
}
