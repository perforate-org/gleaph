//! `gleaph prepared` — file-based prepared-query registration (ADR 0061).
//!
//! Owns the `prepared/` artifact format (one `<name>.gql` per operation, optional `<name>.toml`
//! sidecar), local validation, and the Router transport for `plan` / `status` / `apply` /
//! `drop`. The CLI authors only the name, the kind (classified from the source), and explicit
//! sidecar fields; the Router completes parameter and result metadata and remains the final
//! validator.

use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use gleaph_gql_ic_wire::{GqlWireRows, GqlWireValue};
use gleaph_gql_params::{GqlParams, GqlValue, encode_gql_params, gql_param_value};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, MutationToken, ReadMode};
use gleaph_prepared_api::{
    OperationKind, PreparedManifest, PreparedOperation, PreparedOperationRecord,
    PreparedRegistration, PreparedSortSpec, ResultSchema, SortKey,
};
use gleaph_prepared_runtime::parse_prepared_source;
use gleaph_router_wire::gql_wire_value_to_json;
use serde::{Deserialize, Serialize};

use crate::remote::RemoteTransport;

/// Maximum prepared-source size, matching the migration statement bound (65,536 bytes).
pub const MAX_PREPARED_SOURCE_BYTES: usize = 65_536;

/// Maximum operations per `prepare` batch, mirroring the Router's `MAX_PREPARED_BATCH`
/// (ADR 0061). Keep in sync with the Router constant.
pub const MAX_PREPARED_BATCH: usize = 32;

/// Default artifact directory name.
pub const DEFAULT_PREPARED_DIR: &str = "prepared";

/// Prepared-query directory selection shared by the subcommands.
#[derive(Clone, Debug, clap::Args)]
pub struct PreparedDirArgs {
    /// Prepared-query directory; defaults to `./prepared` (configurable via `[dirs]` in
    /// `gleaph.toml`, ADR 0062).
    #[arg(long, value_name = "PATH")]
    pub dir: Option<PathBuf>,
}

const GQL_EXTENSION: &str = ".gql";
const TOML_EXTENSION: &str = ".toml";

/// Strict explicit-metadata sidecar (`<name>.toml`).
///
/// Parameter types and the result schema are NOT authorable here; the Router completes them from
/// the program (ADR 0061 §6).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedSidecar {
    /// Explicit operation description. When absent, `///` doc comments on the source apply.
    pub description: Option<String>,
    /// Sort keys accepted by the operation.
    #[serde(default)]
    pub allowed_sorts: Vec<SortKey>,
    /// Whether the operation accepts a consistency option.
    #[serde(default)]
    pub supports_consistency: bool,
    /// Whether the operation accepts an idempotency option.
    #[serde(default)]
    pub supports_idempotency: bool,
}

/// One locally validated prepared operation.
#[derive(Clone, Debug)]
pub struct PreparedOperationArtifact {
    /// Operation name; equal to the file stem.
    pub name: String,
    /// Exact UTF-8 source read from `<name>.gql`.
    pub source: String,
    /// Explicit sidecar fields (defaults when no `.toml` is present).
    pub sidecar: PreparedSidecar,
    /// Execution kind classified locally from the source.
    pub kind: OperationKind,
}

/// Failure while reading, validating, or registering local prepared operations.
#[derive(Debug, thiserror::Error)]
pub enum PreparedError {
    #[error("io error: {0}")]
    Io(String),
    #[error("invalid prepared directory {path}: {reason}")]
    Directory { path: String, reason: String },
    #[error("invalid prepared operation name {0:?}; expected [a-z][a-z0-9-]*")]
    Name(String),
    #[error("invalid prepared GQL: {0}")]
    Gql(String),
    #[error("invalid prepared sidecar: {0}")]
    Sidecar(String),
    #[error("remote: {0}")]
    Remote(String),
    #[error("{0}")]
    Message(String),
}

impl From<std::io::Error> for PreparedError {
    fn from(error: std::io::Error) -> Self {
        PreparedError::Io(error.to_string())
    }
}

/// Router prepared-catalog transport. The outer `Result` is a transport failure; the inner
/// `Result` is the decoded Router envelope, so `NotFound` stays distinguishable.
pub trait PreparedTransport {
    fn get_prepared(
        &mut self,
        name: &str,
    ) -> Result<Result<PreparedOperationRecord, RouterError>, String>;
    fn list_prepared(&mut self) -> Result<Result<PreparedManifest, RouterError>, String>;
    fn prepare(
        &mut self,
        operations: &[PreparedRegistration],
    ) -> Result<Result<(), RouterError>, String>;
    fn drop_prepared(&mut self, name: &str) -> Result<Result<(), RouterError>, String>;
    /// Execute one registered read-only operation through the composite-query
    /// `prepared_query` entrypoint. `sort` is intentionally absent (no `--sort` surface).
    fn run_query(
        &mut self,
        name: &str,
        params: Vec<u8>,
        read_mode: &ReadMode,
    ) -> Result<Result<GqlQueryResult, RouterError>, String>;
    /// Execute one standalone authorization statement through the generic `gql_mutate`
    /// entrypoint (ADR 0074 §5: GRANT/REVOKE ride the host control path; there is no
    /// dedicated publication endpoint). The payload result carries no rows.
    fn authorization_statement(
        &mut self,
        statement: &str,
    ) -> Result<Result<(), RouterError>, String>;
}

/// Router transport over the shared IC-agent remote.
pub struct RouterPreparedTransport {
    remote: RemoteTransport,
}

impl RouterPreparedTransport {
    /// Build a transport using the same network and identity conventions as `migration`.
    pub fn connect(
        canister: &str,
        network: &str,
        identity: Option<&Path>,
        fetch_root_key: bool,
    ) -> Result<Self, PreparedError> {
        let remote = RemoteTransport::connect(canister, network, identity, fetch_root_key)
            .map_err(PreparedError::Remote)?;
        Ok(Self { remote })
    }
}

impl PreparedTransport for RouterPreparedTransport {
    fn get_prepared(
        &mut self,
        name: &str,
    ) -> Result<Result<PreparedOperationRecord, RouterError>, String> {
        self.remote.query("get_prepared", &name.to_string())
    }

    fn list_prepared(&mut self) -> Result<Result<PreparedManifest, RouterError>, String> {
        self.remote.query("list_prepared", &Option::<String>::None)
    }

    fn prepare(
        &mut self,
        operations: &[PreparedRegistration],
    ) -> Result<Result<(), RouterError>, String> {
        self.remote.update("prepare", &operations.to_vec())
    }

    fn drop_prepared(&mut self, name: &str) -> Result<Result<(), RouterError>, String> {
        self.remote.update("drop_prepared", &name.to_string())
    }

    fn run_query(
        &mut self,
        name: &str,
        params: Vec<u8>,
        read_mode: &ReadMode,
    ) -> Result<Result<GqlQueryResult, RouterError>, String> {
        // `prepared_query` takes four separate Candid arguments; a tuple passed as one value
        // would encode as a single record, so this must use the multi-argument variant.
        self.remote.query_args(
            "prepared_query",
            (
                &name.to_string(),
                &params,
                &Option::<Vec<PreparedSortSpec>>::None,
                read_mode,
            ),
        )
    }

    fn authorization_statement(
        &mut self,
        statement: &str,
    ) -> Result<Result<(), RouterError>, String> {
        // `gql_mutate` takes three separate Candid arguments (query text, parameter blob,
        // mutation key); the statement carries no parameters. The mutation key derives
        // deterministically from the statement so a retried publish/unpublish is idempotent
        // on the Router's (caller, graph, key) idempotency scope.
        let mutation_key = format!("gleaph-authorization:{statement}");
        let decoded: Result<GqlQueryResult, RouterError> = self.remote.update_args(
            "gql_mutate",
            (&statement.to_string(), &Vec::<u8>::new(), &mutation_key),
        )?;
        Ok(decoded.map(|_| ()))
    }
}

/// Discover and validate every operation in the `prepared/` directory.
///
/// The directory may contain only `<name>.gql` sources and optional `<name>.toml` sidecars;
/// symlinks, subdirectories, and any other file are rejected. A sidecar without its matching
/// source is rejected.
pub fn discover(root: &Path) -> Result<Vec<PreparedOperationArtifact>, PreparedError> {
    let root_meta = fs::symlink_metadata(root).map_err(PreparedError::from)?;
    if !root_meta.is_dir() {
        return Err(PreparedError::Directory {
            path: root.display().to_string(),
            reason: "prepared root is not a directory".into(),
        });
    }

    let mut entries = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| PreparedError::Directory {
            path: entry.path().display().to_string(),
            reason: "entry name is not valid UTF-8".into(),
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(PreparedError::Directory {
                path: entry.path().display().to_string(),
                reason: "symlinks are not allowed".into(),
            });
        }
        if !metadata.is_file() {
            return Err(PreparedError::Directory {
                path: entry.path().display().to_string(),
                reason: "prepared root may contain only operation files".into(),
            });
        }
        if !entries.insert(name.to_owned()) {
            return Err(PreparedError::Directory {
                path: entry.path().display().to_string(),
                reason: format!("duplicate entry {name:?}"),
            });
        }
    }

    for name in &entries {
        if let Some(stem) = name.strip_suffix(GQL_EXTENSION) {
            validate_name(stem)?;
        } else if let Some(stem) = name.strip_suffix(TOML_EXTENSION) {
            validate_name(stem)?;
            if !entries.contains(&format!("{stem}{GQL_EXTENSION}")) {
                return Err(PreparedError::Directory {
                    path: root.display().to_string(),
                    reason: format!("sidecar {name:?} has no matching source"),
                });
            }
        } else {
            return Err(PreparedError::Directory {
                path: root.display().to_string(),
                reason: format!("unexpected file {name:?}"),
            });
        }
    }

    let mut artifacts = Vec::new();
    for name in &entries {
        let Some(stem) = name.strip_suffix(GQL_EXTENSION) else {
            continue;
        };
        let sidecar = if entries.contains(&format!("{stem}{TOML_EXTENSION}")) {
            let text = fs::read_to_string(root.join(format!("{stem}{TOML_EXTENSION}")))
                .map_err(PreparedError::from)?;
            toml::from_str::<PreparedSidecar>(&text)
                .map_err(|error| PreparedError::Sidecar(error.to_string()))?
        } else {
            PreparedSidecar::default()
        };
        let bytes = read_source_checked(&root.join(name))?;
        let source = String::from_utf8(bytes)
            .map_err(|error| PreparedError::Gql(format!("source is not UTF-8: {error}")))?;
        let parsed = parse_prepared_source(&source)
            .map_err(|error| PreparedError::Gql(error.to_string()))?;
        let kind = if parsed.requires_write_path {
            OperationKind::Update
        } else {
            OperationKind::Query
        };
        artifacts.push(PreparedOperationArtifact {
            name: stem.to_owned(),
            source,
            sidecar,
            kind,
        });
    }
    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(artifacts)
}

/// Convert one local artifact to the Router registration envelope.
///
/// The envelope **always** carries `metadata: Some(...)`: `list_prepared` (the codegen input)
/// surfaces metadata-bearing records only, and the Router completes parameters and result
/// columns (ADR 0061 §4/§6).
pub fn build_registration(artifact: &PreparedOperationArtifact) -> PreparedRegistration {
    PreparedRegistration {
        name: artifact.name.clone(),
        query: artifact.source.clone(),
        metadata: Some(PreparedOperation {
            name: artifact.name.clone(),
            description: artifact.sidecar.description.clone(),
            kind: artifact.kind,
            parameters: vec![],
            result: ResultSchema { columns: vec![] },
            supports_consistency: artifact.sidecar.supports_consistency,
            supports_idempotency: artifact.sidecar.supports_idempotency,
            allowed_sorts: artifact.sidecar.allowed_sorts.clone(),
        }),
    }
}

/// Scaffold a new `<name>.gql` source atomically (fails if the file already exists).
pub fn new(
    root: &Path,
    name: &str,
    description: &str,
) -> Result<PreparedOperationArtifact, PreparedError> {
    validate_name(name)?;
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(PreparedError::Directory {
                path: root.display().to_string(),
                reason: "prepared root must not be a symlink".into(),
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(PreparedError::Directory {
                path: root.display().to_string(),
                reason: "prepared root is not a directory".into(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
        }
        Err(error) => return Err(error.into()),
    }
    let description = if description.is_empty() {
        format!("Prepared query {name}")
    } else {
        description.to_owned()
    };
    let source = format!("/// {description}\nMATCH (n) RETURN n\n");
    let path = root.join(format!("{name}{GQL_EXTENSION}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                PreparedError::Directory {
                    path: path.display().to_string(),
                    reason: "operation already exists".into(),
                }
            } else {
                PreparedError::from(error)
            }
        })?;
    file.write_all(source.as_bytes())?;
    file.sync_all()?;
    Ok(PreparedOperationArtifact {
        name: name.to_owned(),
        source,
        sidecar: PreparedSidecar::default(),
        kind: OperationKind::Query,
    })
}

/// Validate and print the local prepared directory without any remote call.
pub fn plan(root: &Path) -> Result<Vec<PreparedOperationArtifact>, PreparedError> {
    discover(root)
}

/// Per-operation comparison of the local directory against Router storage.
pub struct PreparedStatus {
    /// Operations whose stored query and explicit sidecar fields match the local artifact.
    pub up_to_date: Vec<String>,
    /// Operations absent from Router storage.
    pub missing: Vec<String>,
    /// Operations whose stored query or explicit sidecar fields differ.
    pub drift: Vec<String>,
    /// Operations on the Router that are not in the local directory (metadata-bearing only).
    pub remote_only: Vec<String>,
}

/// Compare the local prepared directory with Router storage.
///
/// Router-completed fields (parameters, result columns) are derived and never diffed; only the
/// query bytes and the explicitly authored sidecar fields are compared.
pub fn status<T: PreparedTransport>(
    root: &Path,
    transport: &mut T,
) -> Result<PreparedStatus, PreparedError> {
    let local = discover(root)?;
    let local_names: BTreeSet<String> =
        local.iter().map(|artifact| artifact.name.clone()).collect();
    let mut status = PreparedStatus {
        up_to_date: Vec::new(),
        missing: Vec::new(),
        drift: Vec::new(),
        remote_only: Vec::new(),
    };
    for artifact in &local {
        match transport
            .get_prepared(&artifact.name)
            .map_err(PreparedError::Remote)?
        {
            Err(RouterError::NotFound(_)) => status.missing.push(artifact.name.clone()),
            Err(error) => {
                return Err(PreparedError::Remote(format!(
                    "Router rejected get_prepared: {error:?}"
                )));
            }
            Ok(record) => {
                if prepared_drift(artifact, &record) {
                    status.drift.push(artifact.name.clone());
                } else {
                    status.up_to_date.push(artifact.name.clone());
                }
            }
        }
    }
    match transport.list_prepared().map_err(PreparedError::Remote)? {
        Ok(manifest) => {
            for operation in manifest.operations {
                if !local_names.contains(&operation.name) {
                    status.remote_only.push(operation.name);
                }
            }
        }
        Err(RouterError::NotFound(_)) => {}
        Err(error) => {
            return Err(PreparedError::Remote(format!(
                "Router rejected list_prepared: {error:?}"
            )));
        }
    }
    status.remote_only.sort();
    Ok(status)
}

/// Register every local operation through Router in bounded all-or-nothing batches.
///
/// A missing or empty `prepared/` directory is a no-op. Chunks of 32 mirror the Router batch
/// bound; a chunk failure propagates the Router error (which names the failing operation) and a
/// re-run converges because the upsert is idempotent.
pub fn apply<T: PreparedTransport>(
    root: &Path,
    transport: &mut T,
) -> Result<PreparedApplyOutcome, PreparedError> {
    let artifacts = match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() => discover(root)?,
        Ok(_) => {
            return Err(PreparedError::Directory {
                path: root.display().to_string(),
                reason: "prepared root is not a directory".into(),
            });
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let mut registered = Vec::new();
    for chunk in artifacts.chunks(MAX_PREPARED_BATCH) {
        let operations: Vec<PreparedRegistration> = chunk.iter().map(build_registration).collect();
        match transport
            .prepare(&operations)
            .map_err(PreparedError::Remote)?
        {
            Ok(()) => registered.extend(chunk.iter().map(|artifact| artifact.name.clone())),
            Err(error) => {
                return Err(PreparedError::Remote(format!(
                    "Router rejected prepare: {error:?}"
                )));
            }
        }
    }
    Ok(PreparedApplyOutcome { registered })
}

/// Result of one `apply` run.
#[derive(Debug)]
pub struct PreparedApplyOutcome {
    /// Operations registered by this run.
    pub registered: Vec<String>,
}

/// Remove one named prepared operation from Router storage.
pub fn drop<T: PreparedTransport>(name: &str, transport: &mut T) -> Result<(), PreparedError> {
    validate_name(name)?;
    match transport
        .drop_prepared(name)
        .map_err(PreparedError::Remote)?
    {
        Ok(()) => Ok(()),
        Err(error) => Err(PreparedError::Remote(format!(
            "Router rejected drop_prepared: {error:?}"
        ))),
    }
}

// ──── prepared publish / unpublish (ADR 0074 §5 PUBLIC publication) ────

/// Build the PUBLIC publication statement for one prepared operation.
///
/// Grammar verified against the Router grant execution tests (`gql_grants.rs`
/// `publication_tests::owner_publishes_public_row_and_revoke_removes_exactly_it`): a grant
/// binds `TO PUBLIC`, a revoke removes with `FROM PUBLIC`, and revoking an absent row is an
/// exact-key `RouterError::NotFound`.
fn publication_statement(name: &str, publish: bool) -> Result<String, PreparedError> {
    validate_name(name)?;
    Ok(if publish {
        format!("GRANT EXECUTE ON PREPARED QUERY {name} TO PUBLIC")
    } else {
        format!("REVOKE EXECUTE ON PREPARED QUERY {name} FROM PUBLIC")
    })
}

/// Grant PUBLIC execute on one registered prepared operation (ADR 0074 §1b).
///
/// The caller must own the op's bound graph or hold `PREPARE_REGISTER`, and the op's stored
/// requirement set must be covered by the caller's effective privileges — both enforced by
/// the Router before its single-row write, so this surface adds no local authorization
/// logic.
pub fn publish<T: PreparedTransport>(name: &str, transport: &mut T) -> Result<(), PreparedError> {
    send_authorization(name, true, transport)
}

/// Remove the PUBLIC execute grant, restoring default-deny for the operation.
pub fn unpublish<T: PreparedTransport>(name: &str, transport: &mut T) -> Result<(), PreparedError> {
    send_authorization(name, false, transport)
}

/// Send one publication statement through the authorization entrypoint. Router's typed
/// rejections stay observable: `NotFound` names exactly what is missing (the op or the
/// stored grant row); any other rejection is reported verbatim.
fn send_authorization<T: PreparedTransport>(
    name: &str,
    publish: bool,
    transport: &mut T,
) -> Result<(), PreparedError> {
    let verb = if publish { "publish" } else { "unpublish" };
    let statement = publication_statement(name, publish)?;
    match transport
        .authorization_statement(&statement)
        .map_err(PreparedError::Remote)?
    {
        Ok(()) => Ok(()),
        Err(RouterError::NotFound(reason)) => Err(PreparedError::Message(format!(
            "{verb} failed for {name:?}: not found: {reason}"
        ))),
        Err(error) => Err(PreparedError::Remote(format!(
            "Router rejected {verb} for {name:?}: {error:?}"
        ))),
    }
}

// ──── prepared run ────

/// Parse repeatable `--param NAME=VALUE` arguments into ordered named parameters.
///
/// `VALUE` is one JSON scalar or array (shell quoting supplies the JSON quotes for text).
/// Duplicate names and unparsable values are rejected before any network call.
pub fn parse_run_params(pairs: &[String]) -> Result<GqlParams, PreparedError> {
    let mut params: GqlParams = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let Some((name, value_text)) = pair.split_once('=') else {
            return Err(PreparedError::Message(format!(
                "--param expects NAME=VALUE, got {pair:?}"
            )));
        };
        if name.is_empty() {
            return Err(PreparedError::Message(format!(
                "--param name must not be empty, got {pair:?}"
            )));
        }
        if params.iter().any(|(existing, _)| existing == name) {
            return Err(PreparedError::Message(format!(
                "duplicate --param name {name:?}"
            )));
        }
        let value: serde_json::Value =
            serde_json::from_str(value_text.trim()).map_err(|error| {
                PreparedError::Message(format!(
                    "--param {name}={value_text:?} is not a JSON scalar or array: {error}"
                ))
            })?;
        params.push((name.to_owned(), gql_param_from_json(name, &value)?));
    }
    Ok(params)
}

/// Map one JSON scalar or array into a logical GQL parameter value. Objects are rejected:
/// the run surface accepts scalars and arrays only.
fn gql_param_from_json(name: &str, value: &serde_json::Value) -> Result<GqlValue, PreparedError> {
    let value = match value {
        serde_json::Value::Null => GqlValue::Null,
        serde_json::Value::Bool(inner) => gql_param_value(*inner),
        serde_json::Value::Number(number) => {
            if let Some(signed) = number.as_i64() {
                gql_param_value(signed)
            } else if let Some(unsigned) = number.as_u64() {
                gql_param_value(unsigned)
            } else {
                let float = number.as_f64().ok_or_else(|| {
                    PreparedError::Message(format!("--param {name} is not a representable number"))
                })?;
                gql_param_value(float)
            }
        }
        serde_json::Value::String(inner) => gql_param_value(inner.clone()),
        serde_json::Value::Array(items) => GqlValue::List(
            items
                .iter()
                .map(|item| gql_param_from_json(name, item))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        serde_json::Value::Object(_) => {
            return Err(PreparedError::Message(format!(
                "--param {name}: objects are not accepted; use a JSON scalar or array"
            )));
        }
    };
    Ok(value)
}

/// Encode parsed parameters with the shared `gleaph-gql-params` compact-binary encoder — the
/// exact bytes the Router decodes with `decode_gql_params_blob`.
pub fn encode_run_params(params: GqlParams) -> Result<Vec<u8>, PreparedError> {
    encode_gql_params(params)
        .map_err(|error| PreparedError::Message(format!("parameter encoding failed: {error}")))
}

/// Resolve the read-consistency contract from `--read-mode`: `eventual`, or
/// `at-least <TOKEN>` where TOKEN is the mutation token JSON issued by an idempotent write.
pub fn parse_read_mode(spec: &[String]) -> Result<ReadMode, PreparedError> {
    let spec: Vec<&str> = spec.iter().map(String::as_str).collect();
    match spec.as_slice() {
        ["eventual"] => Ok(ReadMode::Eventual),
        ["at-least"] => Err(PreparedError::Message(
            "--read-mode at-least requires a mutation token argument".into(),
        )),
        ["eventual", extra] => Err(PreparedError::Message(format!(
            "--read-mode eventual takes no extra argument, got {extra:?}"
        ))),
        ["at-least", token] => {
            let token: MutationToken = serde_json::from_str(token.trim()).map_err(|error| {
                PreparedError::Message(format!(
                    "--read-mode at-least token is not valid MutationToken JSON: {error}"
                ))
            })?;
            Ok(ReadMode::AtLeast(token))
        }
        [] => Err(PreparedError::Message(
            "--read-mode requires a value; expected \"eventual\" or \"at-least <TOKEN>\"".into(),
        )),
        [mode, ..] => Err(PreparedError::Message(format!(
            "unknown --read-mode {mode:?}; expected \"eventual\" or \"at-least\""
        ))),
    }
}

/// Execute one registered read-only prepared operation through the transport.
///
/// The Router `NotFound` verdict for an unknown operation surfaces its message verbatim.
pub fn run<T: PreparedTransport>(
    name: &str,
    params_blob: Vec<u8>,
    read_mode: ReadMode,
    transport: &mut T,
) -> Result<GqlQueryResult, PreparedError> {
    validate_name(name)?;
    match transport
        .run_query(name, params_blob, &read_mode)
        .map_err(PreparedError::Remote)?
    {
        Ok(result) => Ok(result),
        Err(error) => Err(PreparedError::Remote(format!(
            "Router rejected prepared_query: {error}"
        ))),
    }
}

/// Render result rows as a simple aligned table (`""` when no rows were materialized).
pub fn render_rows_table(result: &GqlQueryResult) -> Result<String, PreparedError> {
    let Some(blob) = &result.rows_blob else {
        return Ok(String::new());
    };
    let wire = GqlWireRows::decode_blob(blob)
        .map_err(|error| PreparedError::Message(format!("decode rows blob: {error}")))?;
    if wire.rows.is_empty() {
        return Ok(String::new());
    }
    // Column order follows the first row; later rows may only add columns, which extend the
    // header rather than being dropped silently.
    let mut header: Vec<String> = Vec::new();
    let mut cells: Vec<Vec<String>> = Vec::with_capacity(wire.rows.len());
    for row in &wire.rows {
        let mut row_cells = Vec::with_capacity(row.columns.len());
        for (name, value) in &row.columns {
            if !header.iter().any(|existing| existing == name) {
                header.push(name.clone());
            }
            row_cells.push(cell_string(value)?);
        }
        // Pad rows that omit trailing columns so every cell row matches the header width.
        while row_cells.len() < header.len() {
            row_cells.push(String::new());
        }
        cells.push(row_cells);
    }
    let mut widths = vec![0usize; header.len()];
    for (index, name) in header.iter().enumerate() {
        widths[index] = name.chars().count();
    }
    for row in &cells {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let pad = |text: &str, width: usize| -> String {
        let mut out = String::from(text);
        let used = text.chars().count();
        out.extend(std::iter::repeat_n(' ', width - used));
        out
    };
    let mut lines = Vec::with_capacity(cells.len() + 1);
    let header_line: Vec<String> = header
        .iter()
        .enumerate()
        .map(|(index, name)| pad(name, widths[index]))
        .collect();
    lines.push(header_line.join("  "));
    for row in &cells {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(index, cell)| pad(cell, widths[index]))
            .collect();
        lines.push(line.join("  "));
    }
    Ok(lines.join("\n"))
}

fn cell_string(value: &GqlWireValue) -> Result<String, PreparedError> {
    let json = gql_wire_value_to_json(value.clone())
        .map_err(|error| PreparedError::Message(format!("render result value: {error}")))?;
    Ok(match json {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    })
}

/// Render the raw result payload as pretty-printed JSON (`--json`).
pub fn render_json(result: &GqlQueryResult) -> Result<String, PreparedError> {
    serde_json::to_string_pretty(result)
        .map_err(|error| PreparedError::Message(format!("serialize result: {error}")))
}

/// True when the stored record drifts from the local artifact's authored contract.
fn prepared_drift(artifact: &PreparedOperationArtifact, record: &PreparedOperationRecord) -> bool {
    if record.query != artifact.source {
        return true;
    }
    let Some(metadata) = &record.metadata else {
        // The CLI always registers with metadata; a metadata-less record was created outside it.
        return true;
    };
    if metadata.kind != artifact.kind {
        return true;
    }
    if artifact.sidecar.description.is_some()
        && metadata.description != artifact.sidecar.description
    {
        return true;
    }
    if metadata.allowed_sorts != artifact.sidecar.allowed_sorts {
        return true;
    }
    if metadata.supports_consistency != artifact.sidecar.supports_consistency
        || metadata.supports_idempotency != artifact.sidecar.supports_idempotency
    {
        return true;
    }
    false
}

fn validate_name(name: &str) -> Result<(), PreparedError> {
    let mut chars = name.chars();
    let valid = matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(PreparedError::Name(name.to_owned()))
    }
}

fn read_source_checked(path: &Path) -> Result<Vec<u8>, PreparedError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.len() as usize > MAX_PREPARED_SOURCE_BYTES {
        return Err(PreparedError::Gql(format!(
            "prepared source exceeds {MAX_PREPARED_SOURCE_BYTES} bytes"
        )));
    }
    fs::read(path).map_err(PreparedError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gleaph-prepared-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temporary root");
        path
    }

    fn write_source(root: &Path, name: &str, source: &str) {
        fs::write(root.join(format!("{name}{GQL_EXTENSION}")), source).expect("write source");
    }

    fn write_sidecar(root: &Path, name: &str, toml: &str) {
        fs::write(root.join(format!("{name}{TOML_EXTENSION}")), toml).expect("write sidecar");
    }

    fn record(name: &str, source: &str, sidecar: &PreparedSidecar) -> PreparedOperationRecord {
        PreparedOperationRecord {
            query: source.to_owned(),
            metadata: Some(PreparedOperation {
                name: name.to_owned(),
                description: sidecar.description.clone(),
                kind: OperationKind::Query,
                parameters: vec![],
                result: ResultSchema { columns: vec![] },
                supports_consistency: sidecar.supports_consistency,
                supports_idempotency: sidecar.supports_idempotency,
                allowed_sorts: sidecar.allowed_sorts.clone(),
            }),
        }
    }

    struct FakePreparedTransport {
        records: HashMap<String, PreparedOperationRecord>,
        manifest: Result<PreparedManifest, RouterError>,
        prepare_results: VecDeque<Result<(), RouterError>>,
        run_results: VecDeque<Result<GqlQueryResult, RouterError>>,
        authorization_results: VecDeque<Result<(), RouterError>>,
        calls: Vec<String>,
        sent: Vec<Vec<PreparedRegistration>>,
        queries: Vec<(String, Vec<u8>, ReadMode)>,
        statements: Vec<String>,
    }

    impl FakePreparedTransport {
        fn new(records: HashMap<String, PreparedOperationRecord>) -> Self {
            Self {
                records,
                manifest: Err(RouterError::NotFound("no metadata".into())),
                prepare_results: VecDeque::new(),
                run_results: VecDeque::new(),
                authorization_results: VecDeque::new(),
                calls: Vec::new(),
                sent: Vec::new(),
                queries: Vec::new(),
                statements: Vec::new(),
            }
        }

        fn with_manifest(mut self, operations: Vec<String>) -> Self {
            self.manifest = Ok(PreparedManifest {
                manifest_version: 1,
                graph: gleaph_prepared_api::GraphIdentity {
                    id: "default".into(),
                    name: None,
                },
                operations: operations
                    .into_iter()
                    .map(|name| PreparedOperation {
                        name,
                        description: None,
                        kind: OperationKind::Query,
                        parameters: vec![],
                        result: ResultSchema { columns: vec![] },
                        supports_consistency: false,
                        supports_idempotency: false,
                        allowed_sorts: vec![],
                    })
                    .collect(),
            });
            self
        }

        fn with_prepare_results(mut self, results: Vec<Result<(), RouterError>>) -> Self {
            self.prepare_results = results.into();
            self
        }

        fn with_run_results(mut self, results: Vec<Result<GqlQueryResult, RouterError>>) -> Self {
            self.run_results = results.into();
            self
        }
    }

    impl PreparedTransport for FakePreparedTransport {
        fn get_prepared(
            &mut self,
            name: &str,
        ) -> Result<Result<PreparedOperationRecord, RouterError>, String> {
            self.calls.push(format!("get_prepared:{name}"));
            Ok(match self.records.get(name) {
                Some(record) => Ok(record.clone()),
                None => Err(RouterError::NotFound(name.to_owned())),
            })
        }

        fn list_prepared(&mut self) -> Result<Result<PreparedManifest, RouterError>, String> {
            self.calls.push("list_prepared".into());
            Ok(self.manifest.clone())
        }

        fn prepare(
            &mut self,
            operations: &[PreparedRegistration],
        ) -> Result<Result<(), RouterError>, String> {
            self.sent.push(operations.to_vec());
            self.calls.push(format!("prepare:{}", operations.len()));
            Ok(self.prepare_results.pop_front().unwrap_or(Ok(())))
        }

        fn drop_prepared(&mut self, name: &str) -> Result<Result<(), RouterError>, String> {
            self.calls.push(format!("drop_prepared:{name}"));
            Ok(Ok(()))
        }

        fn run_query(
            &mut self,
            name: &str,
            params: Vec<u8>,
            read_mode: &ReadMode,
        ) -> Result<Result<GqlQueryResult, RouterError>, String> {
            self.calls.push(format!("prepared_query:{name}"));
            self.queries
                .push((name.to_owned(), params, read_mode.clone()));
            Ok(self.run_results.pop_front().unwrap_or_else(|| {
                panic!("unexpected prepared_query:{name}; queue a result first")
            }))
        }

        fn authorization_statement(
            &mut self,
            statement: &str,
        ) -> Result<Result<(), RouterError>, String> {
            self.statements.push(statement.to_owned());
            Ok(self.authorization_results.pop_front().unwrap_or(Ok(())))
        }
    }

    #[test]
    fn discover_rejects_symlinks_extra_files_and_invalid_names() {
        let root = temp_root();
        write_source(&root, "good-name", "MATCH (n) RETURN n");
        // Symlink rejection.
        std::os::unix::fs::symlink(root.join("good-name.gql"), root.join("link.gql"))
            .expect("symlink");
        let error = discover(&root).expect_err("symlink must be rejected");
        assert!(error.to_string().contains("symlinks"));
        fs::remove_file(root.join("link.gql")).expect("remove symlink");
        // Extra file rejection.
        write_sidecar(&root, "orphan", "description = \"x\"\n");
        let error = discover(&root).expect_err("orphan sidecar must be rejected");
        assert!(error.to_string().contains("no matching source"));
        fs::remove_file(root.join("orphan.toml")).expect("remove orphan");
        // Invalid name rejection.
        write_source(&root, "BadName", "MATCH (n) RETURN n");
        let error = discover(&root).expect_err("uppercase name must be rejected");
        assert!(matches!(error, PreparedError::Name(_)));
        fs::remove_file(root.join("BadName.gql")).expect("remove bad name");
        // Unexpected file rejection.
        fs::write(root.join("notes.txt"), "x\n").expect("write notes");
        let error = discover(&root).expect_err("notes.txt must be rejected");
        assert!(error.to_string().contains("unexpected"));
    }

    #[test]
    fn discover_loads_sidecar_and_classifies_kind() {
        let root = temp_root();
        write_source(&root, "read-query", "MATCH (n) RETURN n");
        write_source(&root, "write-op", "MATCH (n) RETURN n NEXT INSERT (m)");
        write_sidecar(
            &root,
            "read-query",
            "description = \"Reads things\"\n[[allowed_sorts]]\nkey = \"name\"\nlabel = \"Name\"\n",
        );
        let artifacts = discover(&root).expect("discover");
        assert_eq!(artifacts.len(), 2);
        let read = artifacts
            .iter()
            .find(|artifact| artifact.name == "read-query")
            .expect("read-query");
        assert_eq!(read.kind, OperationKind::Query);
        assert_eq!(read.sidecar.description.as_deref(), Some("Reads things"));
        assert_eq!(read.sidecar.allowed_sorts.len(), 1);
        assert_eq!(read.sidecar.allowed_sorts[0].key, "name");
        let write = artifacts
            .iter()
            .find(|artifact| artifact.name == "write-op")
            .expect("write-op");
        assert_eq!(write.kind, OperationKind::Update);
    }

    #[test]
    fn discover_rejects_invalid_sidecar_toml() {
        let root = temp_root();
        write_source(&root, "op", "MATCH (n) RETURN n");
        write_sidecar(&root, "op", "unknown_field = 1\n");
        let error = discover(&root).expect_err("deny_unknown_fields sidecar must fail");
        assert!(matches!(error, PreparedError::Sidecar(_)));
    }

    #[test]
    fn new_writes_template_atomically_and_rejects_existing() {
        let root = temp_root();
        let artifact = new(&root, "my-op", "My operation").expect("new");
        assert_eq!(artifact.name, "my-op");
        assert_eq!(
            fs::read_to_string(root.join("my-op.gql")).expect("read template"),
            "/// My operation\nMATCH (n) RETURN n\n"
        );
        let error = new(&root, "my-op", "again").expect_err("existing must fail");
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn new_rejects_invalid_names() {
        let root = temp_root();
        for name in ["BadName", "9starts", "with space", ""] {
            assert!(matches!(
                new(&root, name, "").expect_err("invalid name"),
                PreparedError::Name(_)
            ));
        }
    }

    #[test]
    fn status_reports_missing_drift_up_to_date_and_remote_only() {
        let root = temp_root();
        let source = "MATCH (n) RETURN n";
        write_source(&root, "same", source);
        write_source(&root, "changed", source);
        write_source(&root, "absent-here", source);
        let sidecar = PreparedSidecar {
            description: Some("Same op".into()),
            ..PreparedSidecar::default()
        };
        let mut records = HashMap::new();
        records.insert("same".into(), record("same", source, &sidecar));
        records.insert(
            "changed".into(),
            record(
                "changed",
                "MATCH (n) RETURN n AS name",
                &PreparedSidecar::default(),
            ),
        );
        // "same" sidecar description must match storage; give it the same sidecar.
        write_sidecar(&root, "same", "description = \"Same op\"\n");
        let mut transport = FakePreparedTransport::new(records).with_manifest(vec![
            "same".into(),
            "changed".into(),
            "remote-only-op".into(),
        ]);
        let status = status(&root, &mut transport).expect("status");
        assert_eq!(status.up_to_date, vec!["same"]);
        assert_eq!(status.missing, vec!["absent-here"]);
        assert_eq!(status.drift, vec!["changed"]);
        assert_eq!(status.remote_only, vec!["remote-only-op"]);
        // The wrong implementation that never consults get_prepared would leave calls empty.
        for name in ["same", "changed", "absent-here"] {
            assert!(
                transport
                    .calls
                    .iter()
                    .any(|call| call == &format!("get_prepared:{name}")),
                "status must consult get_prepared for {name}"
            );
        }
    }

    #[test]
    fn status_flags_sidecar_only_drift() {
        let root = temp_root();
        let source = "MATCH (n) RETURN n";
        write_source(&root, "op", source);
        // Storage has a sort; the local sidecar differs -> drift even with identical bytes.
        let stored = PreparedSidecar {
            allowed_sorts: vec![SortKey {
                key: "name".into(),
                label: Some("Name".into()),
            }],
            ..PreparedSidecar::default()
        };
        let mut records = HashMap::new();
        records.insert("op".into(), record("op", source, &stored));
        let mut transport = FakePreparedTransport::new(records);
        let status = status(&root, &mut transport).expect("status");
        assert_eq!(status.drift, vec!["op"]);
        assert!(status.up_to_date.is_empty());
    }

    #[test]
    fn status_treats_metadata_less_record_as_drift() {
        let root = temp_root();
        let source = "MATCH (n) RETURN n";
        write_source(&root, "op", source);
        let mut records = HashMap::new();
        records.insert(
            "op".into(),
            PreparedOperationRecord {
                query: source.into(),
                metadata: None,
            },
        );
        let mut transport = FakePreparedTransport::new(records);
        let status = status(&root, &mut transport).expect("status");
        assert_eq!(status.drift, vec!["op"]);
    }

    #[test]
    fn apply_chunks_envelopes_and_converges() {
        let root = temp_root();
        for index in 0..40 {
            write_source(&root, &format!("op-{index:02}"), "MATCH (n) RETURN n");
        }
        let mut transport = FakePreparedTransport::new(HashMap::new());
        let outcome = apply(&root, &mut transport).expect("apply");
        assert_eq!(outcome.registered.len(), 40);
        assert_eq!(
            transport
                .calls
                .iter()
                .filter(|call| call.starts_with("prepare:"))
                .count(),
            2,
            "40 operations must chunk into two prepare calls"
        );
        assert!(transport.calls.contains(&"prepare:32".to_string()));
        assert!(transport.calls.contains(&"prepare:8".to_string()));
        // Every sent envelope carries Some metadata: the CLI never registers with None because
        // list_prepared (the codegen input) surfaces metadata-bearing records only.
        let sent: Vec<&PreparedRegistration> = transport.sent.iter().flatten().collect();
        assert_eq!(sent.len(), 40, "all 40 envelopes must be sent");
        assert!(
            sent.iter()
                .all(|registration| registration.metadata.is_some()),
            "every envelope must carry Some metadata"
        );
    }

    #[test]
    fn apply_on_missing_dir_is_a_no_op() {
        let missing = std::env::temp_dir().join(format!(
            "gleaph-prepared-missing-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut transport = FakePreparedTransport::new(HashMap::new());
        let outcome = apply(&missing, &mut transport).expect("missing dir is a no-op");
        assert!(outcome.registered.is_empty());
        assert!(transport.calls.is_empty());
    }

    #[test]
    fn apply_propagates_batch_error_and_empty_dir_is_no_op() {
        let root = temp_root();
        write_source(&root, "op", "MATCH (n) RETURN n");
        let mut transport =
            FakePreparedTransport::new(HashMap::new()).with_prepare_results(vec![Err(
                RouterError::InvalidArgument("prepared op 'op': boom".into()),
            )]);
        let error = apply(&root, &mut transport).expect_err("batch error must propagate");
        assert!(error.to_string().contains("prepared op 'op'"));

        let empty = temp_root();
        let mut transport = FakePreparedTransport::new(HashMap::new());
        let outcome = apply(&empty, &mut transport).expect("empty dir is a no-op");
        assert!(outcome.registered.is_empty());
        assert!(transport.calls.is_empty());
    }

    #[test]
    fn drop_issues_exactly_one_call() {
        let mut transport = FakePreparedTransport::new(HashMap::new());
        drop("my-op", &mut transport).expect("drop");
        assert_eq!(transport.calls, vec!["drop_prepared:my-op".to_string()]);
    }

    // ──── prepared publish / unpublish ────

    #[test]
    fn publication_statements_match_the_router_grant_grammar() {
        assert_eq!(
            publication_statement("citation-reach", true).expect("grant"),
            "GRANT EXECUTE ON PREPARED QUERY citation-reach TO PUBLIC"
        );
        assert_eq!(
            publication_statement("citation-reach", false).expect("revoke"),
            "REVOKE EXECUTE ON PREPARED QUERY citation-reach FROM PUBLIC"
        );
    }

    #[test]
    fn publication_rejects_invalid_names_before_any_transport_use() {
        let mut transport = FakePreparedTransport::new(HashMap::new());
        assert!(matches!(
            publish("Bad_Name", &mut transport),
            Err(PreparedError::Name(_))
        ));
        assert!(
            transport.statements.is_empty(),
            "no statement may be built or sent for an invalid name"
        );
    }

    #[test]
    fn publish_sends_the_exact_grant_statement_to_the_authorization_entrypoint() {
        let mut transport = FakePreparedTransport::new(HashMap::new());
        publish("shortest-path", &mut transport).expect("publish");
        assert_eq!(
            transport.statements.as_slice(),
            ["GRANT EXECUTE ON PREPARED QUERY shortest-path TO PUBLIC"]
        );
    }

    #[test]
    fn unpublish_sends_the_exact_revoke_statement() {
        let mut transport = FakePreparedTransport::new(HashMap::new());
        unpublish("shortest-path", &mut transport).expect("unpublish");
        assert_eq!(
            transport.statements.as_slice(),
            ["REVOKE EXECUTE ON PREPARED QUERY shortest-path FROM PUBLIC"]
        );
    }

    #[test]
    fn publication_surfaces_router_not_found_with_its_reason() {
        let mut transport = FakePreparedTransport::new(HashMap::new());
        transport
            .authorization_results
            .push_back(Err(RouterError::NotFound(
                "prepared query \"ghost\"".into(),
            )));
        let error = publish("ghost", &mut transport).expect_err("missing op must fail");
        match error {
            PreparedError::Message(text) => {
                assert!(text.contains("not found"), "{text}");
                assert!(text.contains(r#"prepared query "ghost""#), "{text}");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn publication_surfaces_not_authorized_distinctly_from_not_found() {
        let mut transport = FakePreparedTransport::new(HashMap::new());
        transport
            .authorization_results
            .push_back(Err(RouterError::NotAuthorized));
        let error = unpublish("secret-op", &mut transport).expect_err("non-owner must fail");
        let text = error.to_string();
        assert!(text.contains("NotAuthorized"), "{text}");
        assert!(!text.contains("not found"), "{text}");
    }

    #[test]
    fn build_registration_always_sends_some_metadata() {
        let artifact = PreparedOperationArtifact {
            name: "op".into(),
            source: "MATCH (n) RETURN n".into(),
            sidecar: PreparedSidecar::default(),
            kind: OperationKind::Query,
        };
        let registration = build_registration(&artifact);
        let metadata = registration.metadata.expect("metadata must be Some");
        assert_eq!(metadata.name, "op");
        assert_eq!(metadata.kind, OperationKind::Query);
    }

    // ──── prepared run ────

    use gleaph_gql_ic_wire::GqlWireRow;

    /// A result whose materialized rows carry one text column and one numeric column.
    fn sample_rows_result() -> GqlQueryResult {
        let rows = GqlWireRows {
            rows: vec![
                GqlWireRow {
                    columns: vec![
                        (
                            "name".to_owned(),
                            GqlWireValue::Text("Graph databases".into()),
                        ),
                        ("depth".to_owned(), GqlWireValue::Uint64(3)),
                    ],
                },
                GqlWireRow {
                    columns: vec![
                        ("name".to_owned(), GqlWireValue::Text("GQL".into())),
                        ("depth".to_owned(), GqlWireValue::Uint64(1)),
                    ],
                },
            ],
        }
        .encode_blob()
        .expect("encode rows blob");
        GqlQueryResult {
            row_count: 2,
            rows_blob: Some(rows),
            phase: None,
            token: None,
        }
    }

    #[test]
    fn run_returns_result_and_table_renders_aligned_columns() {
        let mut transport = FakePreparedTransport::new(HashMap::new())
            .with_run_results(vec![Ok(sample_rows_result())]);
        let params = encode_run_params(parse_run_params(&[]).expect("no params")).expect("encode");
        let result = run(
            "variable-length-reach",
            params,
            ReadMode::Eventual,
            &mut transport,
        )
        .expect("run");
        assert_eq!(result.row_count, 2);
        assert_eq!(
            transport.calls,
            vec!["prepared_query:variable-length-reach"]
        );

        let table = render_rows_table(&result).expect("table");
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "header plus two rows");
        assert!(lines[0].starts_with("name"));
        // Column alignment: both name cells start at the same offset.
        let name_offset = lines[0].find("name").expect("header name");
        assert_eq!(
            lines[1].find("Graph databases").expect("row cell"),
            name_offset
        );
        assert_eq!(lines[2].find("GQL").expect("second row cell"), name_offset);
    }

    #[test]
    fn run_surfaces_unknown_operation_not_found_verbatim() {
        let missing = "prepared query \"nope\"";
        let mut transport = FakePreparedTransport::new(HashMap::new())
            .with_run_results(vec![Err(RouterError::NotFound(missing.into()))]);
        let error = run("nope", Vec::new(), ReadMode::Eventual, &mut transport)
            .expect_err("unknown operation must fail");
        let rendered = error.to_string();
        assert!(
            rendered.contains(missing),
            "the Router NotFound message must surface verbatim: {rendered}"
        );
    }

    #[test]
    fn run_params_round_trip_through_the_router_codec() {
        let pairs = [
            r#"name="Graph databases""#.to_owned(),
            "depth=3".to_owned(),
            "tags=[\"a\",\"b\"]".to_owned(),
            "ratio=0.5".to_owned(),
            "flag=true".to_owned(),
        ];
        let parsed = parse_run_params(&pairs).expect("parse");
        let blob = encode_run_params(parsed).expect("encode");

        // Decode with the exact Router-side decoder to prove byte-level agreement.
        let decoded = gleaph_gql_ic::decode_gql_params_blob(&blob).expect("router decode");
        assert_eq!(
            decoded.get("name").expect("name param"),
            &GqlValue::Text("Graph databases".into())
        );
        assert_eq!(
            decoded.get("depth").expect("depth param"),
            &GqlValue::Int64(3)
        );
        assert_eq!(
            decoded.get("tags").expect("tags param"),
            &GqlValue::List(vec![GqlValue::Text("a".into()), GqlValue::Text("b".into())])
        );
        assert_eq!(
            decoded.get("ratio").expect("ratio param"),
            &GqlValue::Float64(0.5)
        );
        assert_eq!(
            decoded.get("flag").expect("flag param"),
            &GqlValue::Bool(true)
        );
    }

    #[test]
    fn run_plumbs_at_least_token_into_the_outgoing_call() {
        let token = MutationToken {
            mutation_id: 7,
            shards: vec![gleaph_graph_kernel::plan_exec::MutationTokenShard {
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                label_stats_seq: None,
            }],
        };
        let mut transport = FakePreparedTransport::new(HashMap::new())
            .with_run_results(vec![Err(RouterError::NotAuthorized)]);
        let params = encode_run_params(parse_run_params(&["id=42".to_owned()]).expect("params"))
            .expect("encode");
        let error = run(
            "some-op",
            params.clone(),
            ReadMode::AtLeast(token.clone()),
            &mut transport,
        )
        .expect_err("queued verdict must propagate");
        assert!(error.to_string().contains("not authorized"));

        // The wrong implementation that drops the caller-selected read mode would capture
        // Eventual here instead of the token.
        let (sent_name, sent_params, sent_mode) = transport.queries[0].clone();
        assert_eq!(sent_name, "some-op");
        assert_eq!(sent_params, params);
        assert_eq!(sent_mode, ReadMode::AtLeast(token));
    }

    #[test]
    fn parse_run_params_rejects_invalid_input_before_any_call() {
        // Missing '=' separator.
        assert!(parse_run_params(&["just-a-name".to_owned()]).is_err());
        // Empty parameter name.
        assert!(parse_run_params(&["=1".to_owned()]).is_err());
        // Unparsable value.
        let error = parse_run_params(&["x={not json}".to_owned()]).expect_err("unparsable");
        assert!(error.to_string().contains("not a JSON scalar or array"));
        // Duplicate names.
        let error = parse_run_params(&["x=1".to_owned(), "x=2".to_owned()])
            .expect_err("duplicate must fail");
        assert!(error.to_string().contains("duplicate --param name"));
        // Object values are out of scope.
        let error = parse_run_params(&["x={\"a\":1}".to_owned()]).expect_err("object value");
        assert!(error.to_string().contains("objects are not accepted"));
    }

    #[test]
    fn parse_read_mode_selects_eventual_at_least_and_rejects_unknown() {
        assert_eq!(
            parse_read_mode(&["eventual".to_owned()]).expect("eventual"),
            ReadMode::Eventual
        );
        let token_json = r#"{"mutation_id":9,"shards":[{"shard_id":2,"label_stats_seq":null}]}"#;
        let mode = parse_read_mode(&["at-least".to_owned(), token_json.to_owned()])
            .expect("at-least with token");
        let gleaph_graph_kernel::plan_exec::ReadMode::AtLeast(token) = &mode else {
            panic!("expected AtLeast, got {mode:?}");
        };
        assert_eq!(token.mutation_id, 9);

        assert!(
            parse_read_mode(&["at-least".to_owned()]).is_err(),
            "missing token"
        );
        assert!(
            parse_read_mode(&["strong".to_owned()]).is_err(),
            "unknown mode must be rejected"
        );
        assert!(
            parse_read_mode(&[]).is_err(),
            "an absent --read-mode value must not resolve"
        );
    }

    #[test]
    fn render_json_prints_the_raw_payload_and_empty_results_render_no_table() {
        let json = render_json(&sample_rows_result()).expect("json");
        assert!(json.contains("\"row_count\": 2"), "{json}");
        assert!(json.contains("\"rows_blob\""), "{json}");

        // A count-only result renders no table rather than an empty grid.
        let count_only = GqlQueryResult {
            row_count: 5,
            rows_blob: None,
            phase: None,
            token: None,
        };
        assert_eq!(
            render_rows_table(&count_only).expect("count-only table"),
            String::new()
        );
        let empty_rows = GqlQueryResult {
            row_count: 0,
            rows_blob: Some(GqlWireRows::default().encode_blob().expect("encode")),
            phase: None,
            token: None,
        };
        assert_eq!(
            render_rows_table(&empty_rows).expect("empty table"),
            String::new()
        );
    }
}
