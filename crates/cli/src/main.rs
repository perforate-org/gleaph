//! The top-level Gleaph command-line interface.

use clap::{CommandFactory, Parser, Subcommand};
use config::{ConfigEnv, DirKey, LoadedConfig};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use thiserror::Error;

pub mod config;
pub mod load;
pub mod migration;
pub mod prepared;
pub mod progress;
pub mod remote;

use load::{LoadArgs, LoadError};
use migration::{MigrationDirArgs, MigrationError};
use prepared::PreparedDirArgs;

#[derive(Debug, Error)]
enum CliError {
    #[error(transparent)]
    Codegen(#[from] gleaph_codegen::CodegenError),
    #[error(transparent)]
    Migration(#[from] MigrationError),
    #[error(transparent)]
    Prepared(#[from] prepared::PreparedError),
    #[error(transparent)]
    Load(#[from] LoadError),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    /// Argument parsing or dispatch failures.
    #[error("{0}")]
    Message(String),
}

impl CliError {
    fn exit_code(&self) -> u8 {
        match self {
            CliError::Load(error) => error.exit_code(),
            _ => 1,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "gleaph", about = "Gleaph command-line tools")]
struct Cli {
    #[command(subcommand)]
    command: TopLevelCommand,
}

#[derive(Debug, Subcommand)]
enum TopLevelCommand {
    /// Generate typed prepared-query clients and adapters.
    Codegen(gleaph_codegen::CodegenArgs),
    /// Validate, plan, and apply immutable schema migrations.
    #[command(subcommand)]
    Migration(MigrationCommand),
    /// Load initial vertices and edges into an existing logical graph.
    Load(LoadArgs),
    /// Register prepared queries from local .gql files.
    #[command(subcommand)]
    Prepared(PreparedCommand),
}

#[derive(Debug, Subcommand)]
enum MigrationCommand {
    /// Create and atomically publish the next migration package.
    New(NewMigrationArgs),
    /// Validate and print the local migration chain without remote calls.
    Plan(MigrationDirArgs),
    /// Compare the local chain with Router's durable migration ledger.
    Status(RemoteMigrationArgs),
    /// Apply pending migrations through Router in parent order.
    Apply(RemoteMigrationArgs),
}

#[derive(Debug, Subcommand)]
enum PreparedCommand {
    /// Scaffold a new prepared-query source file.
    New(NewPreparedArgs),
    /// Validate and print the local prepared directory without remote calls.
    Plan(PreparedDirArgs),
    /// Compare the local prepared directory with Router storage.
    Status(RemotePreparedArgs),
    /// Register local prepared operations through Router in bounded batches.
    Apply(RemotePreparedArgs),
    /// Remove one named prepared operation from Router storage.
    Drop(DropPreparedArgs),
}

#[derive(Debug, clap::Args)]
struct RemoteMigrationArgs {
    #[command(flatten)]
    dir: MigrationDirArgs,
    /// Router canister principal (required unless supplied by GLEAPH_CANISTER or `gleaph.toml`).
    #[arg(long, value_name = "PRINCIPAL")]
    canister: Option<String>,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(short = 'n', long, value_name = "NETWORK")]
    network: Option<String>,
    /// PEM file containing a Secp256k1 identity.
    #[arg(long, value_name = "PATH")]
    identity: Option<PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    fetch_root_key: Option<bool>,
}

#[derive(Debug, clap::Args)]
struct RemotePreparedArgs {
    #[command(flatten)]
    dir: PreparedDirArgs,
    /// Router canister principal (required unless supplied by GLEAPH_CANISTER or `gleaph.toml`).
    #[arg(long, value_name = "PRINCIPAL")]
    canister: Option<String>,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(short = 'n', long, value_name = "NETWORK")]
    network: Option<String>,
    /// PEM file containing a Secp256k1 identity.
    #[arg(long, value_name = "PATH")]
    identity: Option<PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    fetch_root_key: Option<bool>,
}

#[derive(Debug, clap::Args)]
struct DropPreparedArgs {
    #[command(flatten)]
    remote: RemotePreparedArgs,
    /// Prepared operation name to remove.
    #[arg(value_name = "NAME")]
    name: String,
}

#[derive(Debug, clap::Args)]
struct NewPreparedArgs {
    #[command(flatten)]
    dir: PreparedDirArgs,
    /// Lowercase kebab-case operation name.
    #[arg(value_name = "NAME")]
    name: String,
    /// Human-readable description emitted as the source doc comment.
    #[arg(long, default_value = "")]
    description: String,
}

#[derive(Debug, clap::Args)]
struct NewMigrationArgs {
    #[command(flatten)]
    dir: MigrationDirArgs,
    /// Lowercase migration slug; the six-digit prefix is derived from the local chain.
    #[arg(value_name = "SLUG")]
    slug: String,
    /// Human-readable migration description.
    #[arg(long, default_value = "")]
    description: String,
    /// Read the migration payload from this path: a single GQL file or a directory of `*.gql`
    /// fragments scaffolded as `up/`. Without it, a minimal graph-type template is created.
    #[arg(long, value_name = "PATH")]
    up: Option<std::path::PathBuf>,
}

/// Run the top-level CLI with argument strings excluding the executable name, without any
/// `GLEAPH_*` environment overrides. Keeping this small adapter allows unit tests to exercise
/// dispatch without spawning a process or depending on the host environment.
pub fn run(args: Vec<String>) -> Result<(), String> {
    run_with_env(args, ConfigEnv::default()).map_err(|error| error.to_string())
}

/// Like [`run`], with an explicit environment snapshot so tests stay hermetic.
fn run_with_env(args: Vec<String>, env: ConfigEnv) -> Result<(), CliError> {
    parse_and_dispatch(args, env)
}

fn parse_and_dispatch(args: Vec<String>, env: ConfigEnv) -> Result<(), CliError> {
    let Some(first) = args.first() else {
        return Err(CliError::Message("a command is required".into()));
    };
    if !matches!(
        first.as_str(),
        "codegen" | "migration" | "load" | "prepared" | "-h" | "--help"
    ) {
        return Err(CliError::Message(format!("unknown command {first:?}")));
    }
    if matches!(first.as_str(), "-h" | "--help") {
        let mut command = Cli::command();
        print!("{}", command.render_help());
        return Ok(());
    }
    let argv = std::iter::once("gleaph".to_owned()).chain(args);
    let cli = Cli::try_parse_from(argv).map_err(|error| CliError::Message(error.to_string()))?;
    let cwd = std::env::current_dir()
        .map_err(|error| CliError::Message(format!("resolve current directory: {error}")))?;
    let loaded = config::Config::load(&cwd, &env)?;
    dispatch(cli.command, &env, loaded.as_ref())
}

fn dispatch(
    command: TopLevelCommand,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
) -> Result<(), CliError> {
    match command {
        TopLevelCommand::Codegen(args) => {
            Ok(gleaph_codegen::run(resolve_codegen(args, env, loaded)?)?)
        }
        TopLevelCommand::Migration(command) => execute_migration(command, env, loaded),
        TopLevelCommand::Load(args) => {
            let args = resolve_load(args, env, loaded)?;
            let outcome = load::execute(&args)?;
            match outcome {
                load::LoadOutcome::Loaded { key } => {
                    println!("bulk load completed (job key: {key})")
                }
                load::LoadOutcome::Skipped { key } => {
                    println!("bulk load already completed; skipped (job key: {key})")
                }
            }
            Ok(())
        }
        TopLevelCommand::Prepared(command) => execute_prepared(command, env, loaded),
    }
}

/// One connection set with the canister required (for `migration`, `prepared`, and `load`).
struct ResolvedRemote {
    canister: String,
    network: String,
    identity: Option<PathBuf>,
    fetch_root_key: bool,
}

fn required_remote(
    canister_flag: Option<&str>,
    network_flag: Option<&str>,
    identity_flag: Option<&std::path::Path>,
    fetch_root_key_flag: Option<bool>,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
) -> Result<ResolvedRemote, CliError> {
    let options = config::merge_remote(
        canister_flag,
        network_flag,
        identity_flag,
        fetch_root_key_flag,
        env,
        loaded,
    )?;
    let canister = match options.canister {
        Some(c) => c,
        // No explicit canister: resolve the Router id from the Account canister
        // (ADR 0068). The explicit `--canister` / GLEAPH_CANISTER / config path is
        // preserved for backward compatibility.
        None => resolve_router_from_account(&options, env, loaded)?,
    };
    Ok(ResolvedRemote {
        canister,
        network: options.network,
        identity: options.identity,
        fetch_root_key: options.fetch_root_key,
    })
}

/// Resolve the Router canister id from the Account canister when no explicit canister is given.
/// Reads the platform-fixed Account id from `.gleaph/data/mappings/<env>.ids.json`, then calls
/// `Account.resolve_router` with the router name (`GLEAPH_ROUTER`, default "default"). The
/// resolved id is cached in `.gleaph/cache/account/<env>.router.json` and reused on subsequent
/// runs.
fn resolve_router_from_account(
    options: &config::RemoteOptions,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
) -> Result<String, CliError> {
    let loaded = loaded.ok_or_else(|| {
        CliError::Message(
            "no gleaph.toml; cannot resolve the Router from the Account canister".into(),
        )
    })?;
    let router_name = env.router.as_deref().unwrap_or("default");
    let environment = config::effective_environment(env, &options.network);
    if let Some(cached) = config::read_router_cache(loaded, &environment) {
        return Ok(cached);
    }
    let mapping = config::read_mapping(loaded, &environment)?;
    let account_canister = mapping.get("account").ok_or_else(|| {
        CliError::Message(
            "no account canister in .gleaph/data/mappings; run `gleaph deploy` first".into(),
        )
    })?;
    let account_principal = candid::Principal::from_text(account_canister)
        .map_err(|e| CliError::Message(format!("invalid account canister id: {e}")))?;
    let transport = remote::RemoteTransport::connect(
        account_canister,
        &options.network,
        options.identity.as_deref(),
        options.fetch_root_key,
    )
    .map_err(CliError::Message)?;
    let router = remote::resolve_router_id(&transport, &account_principal, router_name)
        .map_err(CliError::Message)?;
    let router_text = router.to_text();
    config::write_router_cache(loaded, &environment, &router_text);
    Ok(router_text)
}

/// Merge `gleaph.toml` and `GLEAPH_*` defaults into the parsed codegen args. The manifest source
/// is never created by config: `--manifest` suppresses the deployment canister (ADR 0062 §5).
fn resolve_codegen(
    mut args: gleaph_codegen::CodegenArgs,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
) -> Result<gleaph_codegen::CodegenArgs, CliError> {
    let config = loaded.map(|loaded| &loaded.config);
    // Network, identity, and fetch_root_key follow the shared precedence chain; the canister
    // merge is gated on the caller-selected source below.
    let remote = config::merge_remote(
        None,
        args.network.as_deref(),
        args.identity.as_deref(),
        args.fetch_root_key,
        env,
        loaded,
    )?;
    if args.manifest.is_none() && args.canister.is_none() {
        args.canister = match remote.canister {
            Some(c) => Some(c),
            None => Some(resolve_router_from_account(&remote, env, loaded)?),
        };
    }
    // The graph selects the Router-side manifest source, so `--manifest` suppresses it
    // exactly like it suppresses the deployment canister (ADR 0062 §5).
    if args.manifest.is_none() && args.graph.is_none() {
        args.graph = config
            .and_then(|config| config.codegen())
            .and_then(|codegen| codegen.graph.clone());
    }
    if args.target.is_none() {
        args.target = config
            .and_then(|config| config.codegen())
            .and_then(|codegen| codegen.target.clone());
    }
    if args.output.is_none() {
        let config_output = config
            .and_then(|config| config.codegen())
            .and_then(|codegen| codegen.output.as_ref());
        if let Some((loaded, output)) = loaded.zip(config_output) {
            args.output = Some(config::resolve_config_path(&loaded.path, output));
        }
    }
    args.identity = remote.identity;
    args.fetch_root_key = Some(remote.fetch_root_key);
    args.network = Some(remote.network);
    Ok(args)
}

/// Merge `gleaph.toml` and `GLEAPH_*` defaults into the parsed load args.
fn resolve_load(
    mut args: LoadArgs,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
) -> Result<LoadArgs, CliError> {
    let remote = required_remote(
        args.canister.as_deref(),
        args.network.as_deref(),
        args.identity.as_deref(),
        args.fetch_root_key,
        env,
        loaded,
    )?;
    args.canister = Some(remote.canister);
    args.network = Some(remote.network);
    args.identity = remote.identity;
    args.fetch_root_key = Some(remote.fetch_root_key);
    let config = loaded.map(|loaded| &loaded.config);
    if args.graph.is_none() {
        args.graph = config
            .and_then(|config| config.load_config())
            .and_then(|load| load.graph.clone());
    }
    if args.key.is_none() {
        args.key = config
            .and_then(|config| config.load_config())
            .and_then(|load| load.key.clone());
    }
    if args.state_file.is_none() {
        let config_state = config
            .and_then(|config| config.load_config())
            .and_then(|load| load.state_file.as_ref());
        if let Some((loaded, state)) = loaded.zip(config_state) {
            args.state_file = Some(config::resolve_config_path(&loaded.path, state));
        }
    }
    Ok(args)
}

fn execute_prepared(
    command: PreparedCommand,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
) -> Result<(), CliError> {
    use prepared::RouterPreparedTransport;
    match command {
        PreparedCommand::New(args) => {
            let dir = config::resolved_dir(args.dir.dir.as_deref(), loaded, DirKey::Prepared);
            let artifact = prepared::new(&dir, &args.name, &args.description)?;
            println!(
                "created {} ({})",
                artifact.name,
                dir.join(format!("{}.gql", artifact.name)).display()
            );
            Ok(())
        }
        PreparedCommand::Plan(args) => {
            let dir = config::resolved_dir(args.dir.as_deref(), loaded, DirKey::Prepared);
            let artifacts = prepared::plan(&dir)?;
            if artifacts.is_empty() {
                println!("no prepared operations");
            } else {
                for artifact in artifacts {
                    println!("{} {:?}", artifact.name, artifact.kind);
                }
            }
            Ok(())
        }
        PreparedCommand::Status(args) => {
            let remote = required_remote(
                args.canister.as_deref(),
                args.network.as_deref(),
                args.identity.as_deref(),
                args.fetch_root_key,
                env,
                loaded,
            )?;
            let dir = config::resolved_dir(args.dir.dir.as_deref(), loaded, DirKey::Prepared);
            let mut transport = RouterPreparedTransport::connect(
                &remote.canister,
                &remote.network,
                remote.identity.as_deref(),
                remote.fetch_root_key,
            )?;
            let status = prepared::status(&dir, &mut transport)?;
            for name in &status.missing {
                println!("{name} missing");
            }
            for name in &status.drift {
                println!("{name} drift");
            }
            for name in &status.remote_only {
                println!("{name} remote-only");
            }
            println!(
                "up-to-date {}/{}",
                status.up_to_date.len(),
                status.up_to_date.len() + status.missing.len() + status.drift.len()
            );
            if !status.missing.is_empty() || !status.drift.is_empty() {
                return Err(CliError::Prepared(prepared::PreparedError::Message(
                    "prepared operations are missing or drifted".into(),
                )));
            }
            Ok(())
        }
        PreparedCommand::Apply(args) => {
            let remote = required_remote(
                args.canister.as_deref(),
                args.network.as_deref(),
                args.identity.as_deref(),
                args.fetch_root_key,
                env,
                loaded,
            )?;
            let dir = config::resolved_dir(args.dir.dir.as_deref(), loaded, DirKey::Prepared);
            let mut transport = RouterPreparedTransport::connect(
                &remote.canister,
                &remote.network,
                remote.identity.as_deref(),
                remote.fetch_root_key,
            )?;
            let outcome = prepared::apply(&dir, &mut transport)?;
            if outcome.registered.is_empty() {
                println!("no prepared operations");
            } else {
                for name in &outcome.registered {
                    println!("{name} registered");
                }
            }
            Ok(())
        }
        PreparedCommand::Drop(args) => {
            let remote = required_remote(
                args.remote.canister.as_deref(),
                args.remote.network.as_deref(),
                args.remote.identity.as_deref(),
                args.remote.fetch_root_key,
                env,
                loaded,
            )?;
            let mut transport = RouterPreparedTransport::connect(
                &remote.canister,
                &remote.network,
                remote.identity.as_deref(),
                remote.fetch_root_key,
            )?;
            prepared::drop(&args.name, &mut transport)?;
            println!("dropped {}", args.name);
            Ok(())
        }
    }
}

fn execute_migration(
    command: MigrationCommand,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
) -> Result<(), CliError> {
    match command {
        MigrationCommand::New(args) => {
            let dir = config::resolved_dir(args.dir.dir.as_deref(), loaded, DirKey::Migrations);
            let artifact =
                migration::create_new(&dir, &args.slug, &args.description, args.up.as_deref())?;
            println!("created {} ({})", artifact.id(), artifact.path.display());
            Ok(())
        }
        MigrationCommand::Plan(args) => {
            let dir = config::resolved_dir(args.dir.as_deref(), loaded, DirKey::Migrations);
            let plan = migration::plan(&dir)?;
            if plan.migrations.is_empty() {
                println!("no migrations");
            } else {
                for artifact in plan.migrations {
                    println!("{} {}", artifact.id(), artifact.checksum_hex());
                }
            }
            Ok(())
        }
        MigrationCommand::Status(args) => {
            let remote = required_remote(
                args.canister.as_deref(),
                args.network.as_deref(),
                args.identity.as_deref(),
                args.fetch_root_key,
                env,
                loaded,
            )?;
            let dir = config::resolved_dir(args.dir.dir.as_deref(), loaded, DirKey::Migrations);
            let mut transport = migration::RouterMigrationTransport::connect(
                &remote.canister,
                &remote.network,
                remote.identity.as_deref(),
                remote.fetch_root_key,
            )?;
            let status = migration::status(&dir, &mut transport)?;
            println!("applied {}/{}", status.applied_count, status.total_count);
            Ok(())
        }
        MigrationCommand::Apply(args) => {
            let remote = required_remote(
                args.canister.as_deref(),
                args.network.as_deref(),
                args.identity.as_deref(),
                args.fetch_root_key,
                env,
                loaded,
            )?;
            let dir = config::resolved_dir(args.dir.dir.as_deref(), loaded, DirKey::Migrations);
            let mut transport = migration::RouterMigrationTransport::connect(
                &remote.canister,
                &remote.network,
                remote.identity.as_deref(),
                remote.fetch_root_key,
            )?;
            let mut renderer =
                migration::MigrationProgressRenderer::new(std::io::stdout().is_terminal());
            let outcomes =
                migration::apply(&dir, &mut transport, &mut |view| renderer.render(view))?;
            renderer.close();
            let summary = migration::ApplySummary::from_outcomes(&outcomes);
            match (summary.applied, summary.replay) {
                (0, 0) => println!("no migrations to apply"),
                (applied, 0) => println!("applied {applied} migrations"),
                (applied, replay) => println!("applied {applied} new, {replay} replay"),
            }
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    let result = ConfigEnv::from_process()
        .map_err(CliError::Config)
        .and_then(|env| parse_and_dispatch(std::env::args().skip(1).collect(), env));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("gleaph: {message}");
            ExitCode::from(message.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_codegen, resolve_load, run, run_with_env};
    use crate::config::{ConfigEnv, LoadedConfig};
    use crate::load::LoadArgs;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMPORARY_OUTPUT_ID: AtomicU64 = AtomicU64::new(0);

    fn temporary_output_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let id = NEXT_TEMPORARY_OUTPUT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("gleaph-cli-{}-{nonce}-{id}.ts", std::process::id()))
    }

    fn temporary_root(tag: &str) -> PathBuf {
        let nonce = NEXT_TEMPORARY_OUTPUT_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("gleaph-cli-{}-{nonce}-{tag}", std::process::id()));
        fs::create_dir_all(&root).expect("temporary root");
        root
    }

    fn env_with_config(path: &Path) -> ConfigEnv {
        ConfigEnv {
            config: Some(path.to_string_lossy().into_owned()),
            ..ConfigEnv::default()
        }
    }

    fn loaded_config(path: &Path) -> LoadedConfig {
        crate::config::Config::load(Path::new("."), &env_with_config(path))
            .expect("config must load")
            .expect("config must exist")
    }

    #[test]
    fn codegen_subcommand_uses_the_shared_codegen_cli() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../codegen/fixtures/typescript-basic");
        let output = temporary_output_path();

        run(vec![
            "codegen".into(),
            "--manifest".into(),
            fixture_dir
                .join("manifest.json")
                .to_string_lossy()
                .into_owned(),
            "--target".into(),
            "ts".into(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
        ])
        .expect("codegen subcommand should generate the fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("generated.ts"))
            .expect("TypeScript fixture should exist");
        assert_eq!(generated, expected);
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn rejects_unknown_top_level_command() {
        let error = run(vec!["deploy".into()]).expect_err("unknown commands must fail");
        assert_eq!(error, "unknown command \"deploy\"");
    }

    #[test]
    fn migration_plan_propagates_invalid_migration_errors() {
        let id = NEXT_TEMPORARY_OUTPUT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gleaph-cli-migration-plan-{}-{id}",
            std::process::id()
        ));
        let package = root.join("000001_invalid");
        fs::create_dir_all(&package).expect("temporary migration root");
        fs::write(
            package.join("migration.toml"),
            "format_version = 1\nid = \"000001_invalid\"\ndescription = \"\"\n",
        )
        .expect("manifest");
        fs::write(package.join("up.gql"), "INSERT (n)\n").expect("invalid statement");

        let output = run(vec![
            "migration".into(),
            "plan".into(),
            "--dir".into(),
            root.to_string_lossy().into_owned(),
        ]);
        let error = output.expect_err("invalid migration must propagate through top-level CLI");
        assert!(error.contains("invalid migration GQL"));
        fs::remove_dir_all(root).expect("temporary migration root cleanup");
    }

    #[test]
    fn codegen_target_and_output_merge_from_config_while_manifest_suppresses_remote_source() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../codegen/fixtures/typescript-basic");
        let root = temporary_root("codegen-config");
        let config_path = root.join("gleaph.toml");
        fs::write(
            &config_path,
            "[deployment.ic]\ncanister = \"aaaaa-aa\"\n[codegen]\ntarget = \"typescript\"\ngraph = \"some-graph\"\n",
        )
        .expect("config write");
        let output = temporary_output_path();

        run_with_env(
            vec![
                "codegen".into(),
                "--manifest".into(),
                fixture_dir
                    .join("manifest.json")
                    .to_string_lossy()
                    .into_owned(),
                "--output".into(),
                output.to_string_lossy().into_owned(),
            ],
            env_with_config(&config_path),
        )
        .expect(
            "config target must supply --target; --manifest must suppress config canister and graph",
        );

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("generated.ts"))
            .expect("TypeScript fixture should exist");
        assert_eq!(generated, expected);
        fs::remove_file(output).expect("temporary output cleanup");
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }

    #[test]
    fn remote_commands_report_missing_canister_with_their_sources() {
        for command in ["migration status", "prepared status", "load"] {
            let error = run_with_env(
                command.split_whitespace().map(str::to_owned).collect(),
                ConfigEnv::default(),
            )
            .expect_err("a remote command without any canister source must fail")
            .to_string();
            assert!(
                error.contains("cannot resolve the Router from the Account canister"),
                "{command} must report the unresolved Router: {error}"
            );
        }
    }

    #[test]
    fn config_canister_reaches_transport_validation() {
        let root = temporary_root("config-canister");
        let config_path = root.join("gleaph.toml");
        fs::write(
            &config_path,
            "[deployment.ic]\ncanister = \"not-a-principal\"\n",
        )
        .expect("config write");

        let error = run_with_env(
            vec!["migration".into(), "status".into()],
            env_with_config(&config_path),
        )
        .expect_err("the config canister must be validated by the transport")
        .to_string();
        assert!(
            error.contains("invalid canister principal"),
            "migration status must consume the config canister: {error}"
        );
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }

    #[test]
    fn env_canister_supplies_the_required_connection() {
        let error = run_with_env(
            vec!["prepared".into(), "drop".into(), "some-op".into()],
            ConfigEnv {
                canister: Some("not-a-principal".into()),
                ..ConfigEnv::default()
            },
        )
        .expect_err("GLEAPH_CANISTER must be consumed by the transport")
        .to_string();
        assert!(
            error.contains("invalid canister principal"),
            "prepared drop must consume GLEAPH_CANISTER: {error}"
        );
    }

    fn codegen_args() -> gleaph_codegen::CodegenArgs {
        gleaph_codegen::CodegenArgs {
            manifest: None,
            canister: None,
            graph: None,
            network: None,
            identity: None,
            fetch_root_key: None,
            target: None,
            output: None,
            format: Vec::new(),
        }
    }

    #[test]
    fn resolve_load_merges_load_table_with_config_relative_paths() {
        let root = temporary_root("resolve-load");
        let config_path = root.join("gleaph.toml");
        fs::write(
            &config_path,
            "[deployment.local]\ncanister = \"aaaaa-aa\"\n[load]\ngraph = \"my_graph\"\nkey = \"my-key\"\nstate_file = \".load-state.json\"\n",
        )
        .expect("config write");
        let args = resolve_load(
            LoadArgs {
                artifacts: vec![PathBuf::from("seed.json")],
                canister: None,
                graph: None,
                key: None,
                network: None,
                identity: None,
                fetch_root_key: None,
                format: None,
                vertices: None,
                edges: None,
                fresh: false,
                state_file: None,
            },
            &ConfigEnv {
                network: Some("local".into()),
                ..ConfigEnv::default()
            },
            Some(&loaded_config(&config_path)),
        )
        .expect("merge");
        assert_eq!(args.canister.as_deref(), Some("aaaaa-aa"));
        assert_eq!(args.network.as_deref(), Some("local"));
        assert_eq!(args.graph.as_deref(), Some("my_graph"));
        assert_eq!(args.key.as_deref(), Some("my-key"), "[load] key must apply");
        assert_eq!(
            args.state_file.as_deref(),
            Some(root.join(".load-state.json").as_path()),
            "state_file must resolve against the config directory"
        );
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }

    #[test]
    fn resolve_codegen_merges_url_deployment_and_codegen_table() {
        let root = temporary_root("resolve-codegen");
        let config_path = root.join("gleaph.toml");
        fs::write(
            &config_path,
            "[deployment.\"https://example.com\"]\ncanister = \"aaaaa-aa\"\nidentity = \"staging.pem\"\nfetch_root_key = true\n[codegen]\ntarget = \"typescript\"\noutput = \"out.ts\"\ngraph = \"g\"\n",
        )
        .expect("config write");
        let mut args = codegen_args();
        args.target = Some("javascript".into());
        let args = resolve_codegen(
            args,
            &ConfigEnv {
                network: Some("https://example.com".into()),
                ..ConfigEnv::default()
            },
            Some(&loaded_config(&config_path)),
        )
        .expect("merge");
        assert_eq!(args.canister.as_deref(), Some("aaaaa-aa"));
        assert_eq!(args.network.as_deref(), Some("https://example.com"));
        assert_eq!(
            args.identity.as_deref(),
            Some(root.join("staging.pem").as_path()),
            "identity must resolve against the config directory"
        );
        assert_eq!(
            args.fetch_root_key,
            Some(true),
            "the URL entry must supply fetch_root_key"
        );
        assert_eq!(
            args.target.as_deref(),
            Some("javascript"),
            "the explicit flag must win over [codegen] target"
        );
        assert_eq!(
            args.output.as_deref(),
            Some(root.join("out.ts").as_path()),
            "output must resolve against the config directory"
        );
        assert_eq!(args.graph.as_deref(), Some("g"));
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }

    #[test]
    fn resolve_codegen_never_creates_a_manifest_source() {
        let root = temporary_root("codegen-source");
        let config_path = root.join("gleaph.toml");
        fs::write(&config_path, "[deployment.ic]\ncanister = \"aaaaa-aa\"\n")
            .expect("config write");
        let loaded = loaded_config(&config_path);

        // --manifest suppresses the deployment canister entirely.
        let mut args = codegen_args();
        args.manifest = Some(PathBuf::from("m.json"));
        let args = resolve_codegen(args, &ConfigEnv::default(), Some(&loaded)).expect("merge");
        assert_eq!(
            args.canister, None,
            "--manifest must suppress the deployment canister"
        );

        // Config canister without a config graph still fails IncompleteRemoteSource in run().
        let args =
            resolve_codegen(codegen_args(), &ConfigEnv::default(), Some(&loaded)).expect("merge");
        assert_eq!(args.canister.as_deref(), Some("aaaaa-aa"));
        assert_eq!(args.graph, None);
        let error = gleaph_codegen::run(args).expect_err("canister without graph must fail closed");
        assert_eq!(
            error.to_string(),
            "the Router manifest source needs --canister and --graph; missing graph (set --graph or [codegen] graph)"
        );
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }

    #[test]
    fn resolve_codegen_url_entry_omitting_fetch_root_key_fails_closed() {
        let root = temporary_root("codegen-url-nofetch");
        let config_path = root.join("gleaph.toml");
        fs::write(
            &config_path,
            "[deployment.\"https://example.com\"]\ncanister = \"aaaaa-aa\"\n",
        )
        .expect("config write");
        let mut args = codegen_args();
        args.graph = Some("g".into());
        args.target = Some("typescript".into());
        let args = resolve_codegen(
            args,
            &ConfigEnv {
                network: Some("https://example.com".into()),
                ..ConfigEnv::default()
            },
            Some(&loaded_config(&config_path)),
        )
        .expect("merge");
        assert_eq!(args.fetch_root_key, Some(false));
        let error = gleaph_codegen::run(args).expect_err("omitted fetch_root_key must fail closed");
        assert!(
            error.to_string().contains("requires --fetch-root-key"),
            "the existing root-key-required error must fire: {error}"
        );
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }
}
