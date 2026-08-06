//! Command-line implementation shared by `gleaph-codegen` and `gleaph codegen`.

use candid::{Decode, Encode, Principal};
use clap::Args;
use gleaph_graph_kernel::federation::RouterError;
use ic_agent::identity::Secp256k1Identity;
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

use crate::{
    RustFormatMode, format_rust, generate_javascript, generate_motoko, generate_rust,
    generate_rust_canister, generate_typescript, validate_manifest,
};

const DEFAULT_IC_URL: &str = "https://icp-api.io";
const DEFAULT_LOCAL_URL: &str = "http://localhost:8000";

/// Arguments shared by the standalone and top-level codegen commands.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct CodegenArgs {
    /// Read a prepared manifest from a local JSON file.
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,
    /// Query a Router canister for the prepared manifest.
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: Option<String>,
    /// Graph name used with `--canister`.
    #[arg(long, value_name = "NAME")]
    pub graph: Option<String>,
    /// Network name (`ic` or `local`) or an HTTP(S) endpoint URL.
    #[arg(short = 'n', long, value_name = "NETWORK")]
    pub network: Option<String>,
    /// PEM file containing a Secp256k1 identity for Router queries.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub fetch_root_key: Option<bool>,
    /// Output profile (`typescript`, `javascript`, `rust`, `rust-canister`, or `motoko`).
    #[arg(long, value_name = "TARGET")]
    pub target: Option<String>,
    /// Write generated source to this path instead of stdout.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Rust formatting policy, for example `rust=never`.
    #[arg(long, value_name = "LANGUAGE=MODE")]
    pub format: Vec<String>,
}

/// Errors produced while loading a manifest or rendering generated source.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CodegenError {
    /// Both local and remote manifest sources were selected.
    #[error("--manifest and --canister are mutually exclusive")]
    ConflictingManifestSources,
    /// A remote manifest source was not given both required fields.
    #[error("the Router manifest source needs --canister and --graph; missing {missing}")]
    IncompleteRemoteSource {
        /// The missing field with actionable sources.
        missing: &'static str,
    },
    /// No manifest source was selected.
    #[error("one manifest source is required: --manifest or --canister/--graph")]
    MissingManifestSource,
    /// No output profile was selected by a programmatic caller.
    #[error("--target is required")]
    MissingTarget,
    /// The selected target is not supported.
    #[error(
        "unsupported target {0:?}; expected typescript, javascript, rust, rust-canister, or motoko"
    )]
    UnsupportedTarget(String),
    /// A format option did not use the `language=mode` shape.
    #[error("invalid format {0:?}; expected rust=<auto|rustfmt|never>")]
    InvalidFormat(String),
    /// A Rust formatting mode is not supported.
    #[error("{0}")]
    InvalidRustFormatMode(String),
    /// A format option named a language other than Rust.
    #[error("unsupported format language {0:?}; expected rust")]
    UnsupportedFormatLanguage(String),
    /// More than one format option selected the same language.
    #[error("duplicate format target {0:?}")]
    DuplicateFormatTarget(String),
    /// A custom network URL did not opt into root-key fetching.
    #[error("a custom network URL requires --fetch-root-key (icp-cli --root-key fetch equivalent)")]
    CustomNetworkRootKeyRequired,
    /// The network selector is not a supported name or URL.
    #[error("unknown network {0:?}; expected \"ic\", \"local\", or an http(s) URL")]
    UnknownNetwork(String),
    /// A local manifest could not be read.
    #[error("read {path}: {error}")]
    ReadManifest {
        /// Path of the manifest that was read.
        path: PathBuf,
        /// Operating-system error text.
        error: String,
    },
    /// An asynchronous runtime could not be created for a remote query.
    #[error("create async runtime: {0}")]
    CreateRuntime(String),
    /// The supplied Router canister principal is invalid.
    #[error("invalid canister principal {canister:?}: {error}")]
    InvalidCanisterPrincipal {
        /// Principal text supplied by the caller.
        canister: String,
        /// Principal parser error text.
        error: String,
    },
    /// The identity PEM file could not be read or decoded.
    #[error("read identity {path}: {error}")]
    ReadIdentity {
        /// Path of the identity file that was read.
        path: PathBuf,
        /// Identity parser error text.
        error: String,
    },
    /// The IC agent could not be constructed.
    #[error("create IC agent: {0}")]
    CreateAgent(String),
    /// The network root key could not be fetched.
    #[error("fetch IC root key: {0}")]
    FetchRootKey(String),
    /// The graph name could not be encoded for the Router query.
    #[error("encode graph name: {0}")]
    EncodeGraph(String),
    /// The Router query failed.
    #[error("query list_prepared: {0}")]
    QueryListPrepared(String),
    /// The Router response envelope could not be decoded.
    #[error("decode list_prepared response: {0}")]
    DecodeResponse(String),
    /// The Router returned a rejected result variant.
    #[error("Router rejected list_prepared: {0}")]
    RouterRejected(String),
    /// The manifest failed schema or semantic validation.
    #[error(transparent)]
    Manifest(#[from] crate::ManifestError),
    /// Generated Rust could not be formatted.
    #[error("format generated Rust: {0}")]
    FormatRust(String),
    /// Generated source could not be written.
    #[error("write {path}: {error}")]
    WriteOutput {
        /// Path of the output file that was written.
        path: PathBuf,
        /// Operating-system error text.
        error: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Typescript,
    Javascript,
    Rust,
    RustCanister,
    Motoko,
}

fn parse_target(target: &str) -> Result<Target, CodegenError> {
    match target {
        "typescript" | "ts" => Ok(Target::Typescript),
        "javascript" | "js" => Ok(Target::Javascript),
        "rust" | "rs" => Ok(Target::Rust),
        "rust-canister" => Ok(Target::RustCanister),
        "motoko" | "mo" => Ok(Target::Motoko),
        other => Err(CodegenError::UnsupportedTarget(other.to_owned())),
    }
}

fn parse_format(formats: &[String]) -> Result<RustFormatMode, CodegenError> {
    let mut rust_format = RustFormatMode::Auto;
    let mut format_targets = std::collections::BTreeSet::new();
    for format in formats {
        let (language, mode) = format
            .split_once('=')
            .ok_or_else(|| CodegenError::InvalidFormat(format.clone()))?;
        if language != "rust" {
            return Err(CodegenError::UnsupportedFormatLanguage(language.to_owned()));
        }
        if !format_targets.insert(language.to_owned()) {
            return Err(CodegenError::DuplicateFormatTarget(language.to_owned()));
        }
        rust_format = RustFormatMode::parse(mode).map_err(CodegenError::InvalidRustFormatMode)?;
    }
    Ok(rust_format)
}

/// Run code generation with parsed [`CodegenArgs`].
pub fn run(args: CodegenArgs) -> Result<(), CodegenError> {
    let rust_format = parse_format(&args.format)?;
    if args.manifest.is_some() && args.canister.is_some() {
        return Err(CodegenError::ConflictingManifestSources);
    }
    if args.canister.is_some() != args.graph.is_some() {
        let missing = if args.canister.is_none() {
            "canister (set --canister or GLEAPH_CANISTER)"
        } else {
            "graph (set --graph or [codegen] graph)"
        };
        return Err(CodegenError::IncompleteRemoteSource { missing });
    }
    let target_text = args.target.as_deref().ok_or(CodegenError::MissingTarget)?;
    if target_text.trim().is_empty() {
        return Err(CodegenError::MissingTarget);
    }
    let target = parse_target(target_text)?;
    let manifest = if let Some(manifest_path) = args.manifest {
        let input =
            fs::read_to_string(&manifest_path).map_err(|error| CodegenError::ReadManifest {
                path: manifest_path,
                error: error.to_string(),
            })?;
        serde_json::from_str(&input).map_err(|error| {
            CodegenError::Manifest(crate::ManifestError::Json(error.to_string()))
        })?
    } else if let Some(canister) = args.canister {
        let graph = args
            .graph
            .as_deref()
            .ok_or(CodegenError::IncompleteRemoteSource { missing: "graph" })?;
        tokio::runtime::Runtime::new()
            .map_err(|error| CodegenError::CreateRuntime(error.to_string()))?
            .block_on(fetch_manifest(
                &canister,
                graph,
                args.network.as_deref().unwrap_or("ic"),
                args.fetch_root_key.unwrap_or(false),
                args.identity.as_deref(),
            ))?
    } else {
        return Err(CodegenError::MissingManifestSource);
    };
    validate_manifest(&manifest)?;
    let generated = match target {
        Target::Typescript => generate_typescript(&manifest),
        Target::Javascript => generate_javascript(&manifest),
        Target::Rust => generate_rust(&manifest),
        Target::RustCanister => generate_rust_canister(&manifest),
        Target::Motoko => generate_motoko(&manifest),
    }?;
    let generated = if matches!(target, Target::Rust | Target::RustCanister) {
        format_rust(generated, rust_format, args.output.as_deref())
            .map_err(CodegenError::FormatRust)?
    } else {
        generated
    };
    if let Some(output) = args.output {
        fs::write(&output, generated).map_err(|error| CodegenError::WriteOutput {
            path: output,
            error: error.to_string(),
        })?;
    } else {
        print!("{generated}");
    }
    Ok(())
}

async fn fetch_manifest(
    canister: &str,
    graph: &str,
    network: &str,
    fetch_root_key_flag: bool,
    identity_path: Option<&std::path::Path>,
) -> Result<gleaph_prepared_api::PreparedManifest, CodegenError> {
    let (url, fetch_root_key) = resolve_network(network, fetch_root_key_flag)?;
    let canister_id =
        Principal::from_text(canister).map_err(|error| CodegenError::InvalidCanisterPrincipal {
            canister: canister.to_owned(),
            error: error.to_string(),
        })?;
    let agent = if let Some(identity_path) = identity_path {
        let identity = Secp256k1Identity::from_pem_file(identity_path).map_err(|error| {
            CodegenError::ReadIdentity {
                path: identity_path.to_owned(),
                error: error.to_string(),
            }
        })?;
        ic_agent::Agent::builder()
            .with_url(url)
            .with_identity(identity)
            .build()
            .map_err(|error| CodegenError::CreateAgent(error.to_string()))?
    } else {
        ic_agent::Agent::builder()
            .with_url(url)
            .build()
            .map_err(|error| CodegenError::CreateAgent(error.to_string()))?
    };
    if fetch_root_key {
        agent
            .fetch_root_key()
            .await
            .map_err(|error| CodegenError::FetchRootKey(error.to_string()))?;
    }
    let args = Encode!(&graph.to_string())
        .map_err(|error| CodegenError::EncodeGraph(error.to_string()))?;
    let response = agent
        .query(&canister_id, "list_prepared")
        .with_arg(args)
        .call()
        .await
        .map_err(|error| CodegenError::QueryListPrepared(error.to_string()))?;
    decode_manifest_response(&response)
}

fn resolve_network(network: &str, fetch_root_key_flag: bool) -> Result<(&str, bool), CodegenError> {
    match network {
        "ic" => Ok((DEFAULT_IC_URL, false)),
        "local" => Ok((DEFAULT_LOCAL_URL, true)),
        url if url.starts_with("http://") || url.starts_with("https://") => {
            if !fetch_root_key_flag {
                return Err(CodegenError::CustomNetworkRootKeyRequired);
            }
            Ok((url, true))
        }
        other => Err(CodegenError::UnknownNetwork(other.to_owned())),
    }
}

fn decode_manifest_response(
    response: &[u8],
) -> Result<gleaph_prepared_api::PreparedManifest, CodegenError> {
    // Decode the `Result` envelope directly. Round-tripping through `IDLArgs`/`IDLValue` is
    // lossy: it re-encodes `None` options as `opt empty` and collapses variants to their single
    // observed member, so `Decode!` can no longer match record types that contain options or
    // multi-member variants (a manifest with prepared operations always does).
    match Decode!(response, Result<gleaph_prepared_api::PreparedManifest, RouterError>)
        .map_err(|error| CodegenError::DecodeResponse(error.to_string()))?
    {
        Ok(manifest) => Ok(manifest),
        Err(error) => Err(CodegenError::RouterRejected(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::{CodegenArgs, run};
    use candid::Encode;
    use clap::Parser;
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_prepared_api::{
        Column, GraphIdentity, MANIFEST_VERSION, OperationKind, Parameter, PreparedManifest,
        PreparedOperation, ResultSchema, SemanticType, SortKey,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMPORARY_OUTPUT_ID: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(flatten)]
        args: CodegenArgs,
    }

    fn parse_args(arguments: &[&str]) -> CodegenArgs {
        let mut argv = vec!["gleaph-codegen"];
        argv.extend(arguments.iter().copied());
        TestCli::try_parse_from(argv)
            .expect("test arguments should satisfy clap parsing")
            .args
    }

    fn temporary_output_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let id = NEXT_TEMPORARY_OUTPUT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gleaph-codegen-cli-{}-{nonce}-{id}.ts",
            std::process::id()
        ))
    }

    #[test]
    fn clap_arguments_preserve_shared_option_shape() {
        let args = parse_args(&[
            "--manifest",
            "manifest.json",
            "--target",
            "ts",
            "-n",
            "local",
            "--identity",
            "identity.pem",
            "--fetch-root-key",
            "--format",
            "rust=never",
        ]);

        assert_eq!(args.target, Some("ts".to_owned()));
        assert_eq!(args.network, Some("local".to_owned()));
        assert_eq!(args.identity, Some(PathBuf::from("identity.pem")));
        assert_eq!(args.fetch_root_key, Some(true));
        assert_eq!(args.format, vec!["rust=never"]);
    }

    #[test]
    fn rejects_missing_target() {
        let error = run(parse_args(&["--manifest", "manifest.json"]))
            .expect_err("a missing output profile must fail after the merge layer");

        assert_eq!(error.to_string(), "--target is required");
    }

    #[test]
    fn generates_typescript_alias_to_explicit_output() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/typescript-basic");
        let manifest = fixture_dir.join("manifest.json");
        let output = temporary_output_path();

        run(parse_args(&[
            "--manifest",
            manifest.to_string_lossy().as_ref(),
            "--target",
            "ts",
            "--output",
            output.to_string_lossy().as_ref(),
        ]))
        .expect("CLI should generate the TypeScript fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("generated.ts"))
            .expect("TypeScript fixture should exist");
        assert_eq!(generated, expected);
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn generates_typescript_imports_required_by_advanced_fixture() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/typescript-advanced");
        let output = temporary_output_path();

        let manifest = fixture_dir.join("manifest.json");
        run(parse_args(&[
            "--manifest",
            manifest.to_string_lossy().as_ref(),
            "--target",
            "typescript",
            "--output",
            output.to_string_lossy().as_ref(),
        ]))
        .expect("CLI should generate the advanced TypeScript fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("generated.ts"))
            .expect("advanced TypeScript fixture should exist");
        assert_eq!(generated, expected);
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn generates_rust_client_fixture() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/rust-client-basic");
        let output = temporary_output_path();

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/typescript-basic/manifest.json");
        run(parse_args(&[
            "--manifest",
            manifest.to_string_lossy().as_ref(),
            "--target",
            "rust",
            "--format",
            "rust=never",
            "--output",
            output.to_string_lossy().as_ref(),
        ]))
        .expect("CLI should generate the Rust client fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("src/lib.rs"))
            .expect("Rust client fixture should exist");
        assert_eq!(generated, expected);
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn generates_motoko_fixture() {
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/motoko-basic");
        let output = temporary_output_path();

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/typescript-basic/manifest.json");
        run(parse_args(&[
            "--manifest",
            manifest.to_string_lossy().as_ref(),
            "--target",
            "motoko",
            "--output",
            output.to_string_lossy().as_ref(),
        ]))
        .expect("CLI should generate the Motoko fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("src/generated.mo"))
            .expect("Motoko fixture should exist");
        assert_eq!(generated, expected);
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn rejects_duplicate_format_targets() {
        let error = run(parse_args(&[
            "--manifest",
            "manifest.json",
            "--target",
            "rust",
            "--format",
            "rust=never",
            "--format",
            "rust=auto",
        ]))
        .expect_err("duplicate format targets must fail");

        assert_eq!(error.to_string(), "duplicate format target \"rust\"");
    }

    #[test]
    fn rejects_unknown_rust_format_mode() {
        let error = run(parse_args(&[
            "--manifest",
            "manifest.json",
            "--target",
            "rust",
            "--format",
            "rust=prettyplease",
        ]))
        .expect_err("unknown Rust format modes must fail");

        assert!(
            error
                .to_string()
                .contains("expected auto, rustfmt, or never")
        );
    }

    #[test]
    fn rejects_unknown_target() {
        let error = run(parse_args(&[
            "--manifest",
            "manifest.json",
            "--target",
            "swift",
        ]))
        .expect_err("unknown targets must fail before reading the manifest");

        assert!(error.to_string().contains("unsupported target \"swift\""));
    }

    #[test]
    fn rejects_incomplete_remote_source() {
        let error = run(parse_args(&[
            "--canister",
            "aaaaa-aa",
            "--target",
            "typescript",
        ]))
        .expect_err("remote source requires a graph name");

        assert_eq!(
            error.to_string(),
            "the Router manifest source needs --canister and --graph; missing graph (set --graph or [codegen] graph)"
        );
    }

    #[test]
    fn rejects_multiple_manifest_sources() {
        let error = run(parse_args(&[
            "--manifest",
            "manifest.json",
            "--canister",
            "aaaaa-aa",
            "--graph",
            "default",
            "--target",
            "typescript",
        ]))
        .expect_err("manifest sources must be mutually exclusive");

        assert_eq!(
            error.to_string(),
            "--manifest and --canister are mutually exclusive"
        );
    }

    #[test]
    fn decodes_router_manifest_response_with_operations_and_semantic_types() {
        // Regression: the response must decode directly as `Result<_, RouterError>`. The previous
        // `IDLArgs`/`IDLValue` round-trip re-encoded `None` options as `opt empty` and collapsed
        // multi-member variants (`OperationKind`, `SemanticType`) to their single observed member,
        // so any manifest carrying prepared operations failed to decode.
        let manifest = PreparedManifest {
            manifest_version: MANIFEST_VERSION,
            graph: GraphIdentity {
                id: "default".into(),
                name: Some("Default".into()),
            },
            operations: vec![PreparedOperation {
                name: "find-users".into(),
                description: Some("Find users by their search term.".into()),
                kind: OperationKind::Query,
                parameters: vec![Parameter {
                    name: "term".into(),
                    description: None,
                    required: true,
                    nullable: true,
                    semantic_type: SemanticType::Text,
                }],
                result: ResultSchema {
                    columns: vec![Column {
                        name: "user_id".into(),
                        semantic_type: SemanticType::Uint64,
                        nullable: false,
                    }],
                },
                supports_consistency: true,
                supports_idempotency: false,
                allowed_sorts: vec![SortKey {
                    key: "name".into(),
                    label: None,
                }],
            }],
        };
        let response = Encode!(&Result::<PreparedManifest, RouterError>::Ok(
            manifest.clone()
        ))
        .expect("manifest response should encode");

        let decoded = super::decode_manifest_response(&response).expect("manifest should decode");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn resolves_icp_cli_network_names() {
        assert_eq!(
            super::resolve_network("ic", false).unwrap(),
            (super::DEFAULT_IC_URL, false)
        );
        assert_eq!(
            super::resolve_network("local", false).unwrap(),
            (super::DEFAULT_LOCAL_URL, true)
        );
    }

    #[test]
    fn custom_network_url_requires_root_key_fetch() {
        let error = super::resolve_network("http://127.0.0.1:8000", false)
            .expect_err("custom networks must opt into fetched root keys");
        assert!(error.to_string().contains("requires --fetch-root-key"));
    }
}
