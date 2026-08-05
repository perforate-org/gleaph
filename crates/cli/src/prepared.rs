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

use gleaph_graph_kernel::federation::RouterError;
use gleaph_prepared_api::{
    OperationKind, PreparedManifest, PreparedOperation, PreparedOperationRecord,
    PreparedRegistration, ResultSchema, SortKey,
};
use gleaph_prepared_runtime::parse_prepared_source;
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
        calls: Vec<String>,
        sent: Vec<Vec<PreparedRegistration>>,
    }

    impl FakePreparedTransport {
        fn new(records: HashMap<String, PreparedOperationRecord>) -> Self {
            Self {
                records,
                manifest: Err(RouterError::NotFound("no metadata".into())),
                prepare_results: VecDeque::new(),
                calls: Vec::new(),
                sent: Vec::new(),
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
}
