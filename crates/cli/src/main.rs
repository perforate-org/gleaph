//! The top-level Gleaph command-line interface.

use clap::{CommandFactory, Parser, Subcommand};
use std::process::ExitCode;
use thiserror::Error;

pub mod load;
pub mod migration;
pub mod prepared;
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
    /// Router canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    canister: String,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(short = 'n', long, default_value = "ic", value_name = "NETWORK")]
    network: String,
    /// PEM file containing a Secp256k1 identity.
    #[arg(long, value_name = "PATH")]
    identity: Option<std::path::PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long)]
    fetch_root_key: bool,
}

#[derive(Debug, clap::Args)]
struct RemotePreparedArgs {
    #[command(flatten)]
    dir: PreparedDirArgs,
    /// Router canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    canister: String,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(short = 'n', long, default_value = "ic", value_name = "NETWORK")]
    network: String,
    /// PEM file containing a Secp256k1 identity.
    #[arg(long, value_name = "PATH")]
    identity: Option<std::path::PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long)]
    fetch_root_key: bool,
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
    /// Read up.gql bytes from this path. Without it, a minimal graph-type template is created.
    #[arg(long, value_name = "PATH")]
    up: Option<std::path::PathBuf>,
}

/// Run the top-level CLI with argument strings excluding the executable name.
///
/// Keeping this small adapter allows unit tests to exercise dispatch without spawning a process.
pub fn run(args: Vec<String>) -> Result<(), String> {
    parse_and_dispatch(args).map_err(|error| error.to_string())
}

fn parse_and_dispatch(args: Vec<String>) -> Result<(), CliError> {
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
    dispatch(cli.command)
}

fn dispatch(command: TopLevelCommand) -> Result<(), CliError> {
    match command {
        TopLevelCommand::Codegen(args) => Ok(gleaph_codegen::run(args)?),
        TopLevelCommand::Migration(command) => Ok(execute_migration(command)?),
        TopLevelCommand::Load(args) => {
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
        TopLevelCommand::Prepared(command) => Ok(execute_prepared(command)?),
    }
}

fn execute_prepared(command: PreparedCommand) -> Result<(), prepared::PreparedError> {
    use prepared::{PreparedError, RouterPreparedTransport};
    match command {
        PreparedCommand::New(args) => {
            let artifact = prepared::new(&args.dir.dir, &args.name, &args.description)?;
            println!(
                "created {} ({})",
                artifact.name,
                args.dir
                    .dir
                    .join(format!("{}.gql", artifact.name))
                    .display()
            );
            Ok(())
        }
        PreparedCommand::Plan(args) => {
            let artifacts = prepared::plan(&args.dir)?;
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
            let mut transport = RouterPreparedTransport::connect(
                &args.canister,
                &args.network,
                args.identity.as_deref(),
                args.fetch_root_key,
            )?;
            let status = prepared::status(&args.dir.dir, &mut transport)?;
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
                return Err(PreparedError::Message(
                    "prepared operations are missing or drifted".into(),
                ));
            }
            Ok(())
        }
        PreparedCommand::Apply(args) => {
            let mut transport = RouterPreparedTransport::connect(
                &args.canister,
                &args.network,
                args.identity.as_deref(),
                args.fetch_root_key,
            )?;
            let outcome = prepared::apply(&args.dir.dir, &mut transport)?;
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
            let mut transport = RouterPreparedTransport::connect(
                &args.remote.canister,
                &args.remote.network,
                args.remote.identity.as_deref(),
                args.remote.fetch_root_key,
            )?;
            prepared::drop(&args.name, &mut transport)?;
            println!("dropped {}", args.name);
            Ok(())
        }
    }
}

fn execute_migration(command: MigrationCommand) -> Result<(), MigrationError> {
    match command {
        MigrationCommand::New(args) => {
            let artifact = migration::create_new(
                &args.dir.dir,
                &args.slug,
                &args.description,
                args.up.as_deref(),
            )?;
            println!("created {} ({})", artifact.id(), artifact.path.display());
            Ok(())
        }
        MigrationCommand::Plan(args) => {
            let plan = migration::plan(&args.dir)?;
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
            let mut transport = migration::RouterMigrationTransport::connect(
                &args.canister,
                &args.network,
                args.identity.as_deref(),
                args.fetch_root_key,
            )?;
            let status = migration::status(&args.dir.dir, &mut transport)?;
            println!("applied {}/{}", status.applied_count, status.total_count);
            Ok(())
        }
        MigrationCommand::Apply(args) => {
            let mut transport = migration::RouterMigrationTransport::connect(
                &args.canister,
                &args.network,
                args.identity.as_deref(),
                args.fetch_root_key,
            )?;
            for outcome in migration::apply(&args.dir.dir, &mut transport)? {
                println!("{outcome}");
            }
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match parse_and_dispatch(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("gleaph: {message}");
            ExitCode::from(message.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use std::fs;
    use std::path::PathBuf;
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
}
