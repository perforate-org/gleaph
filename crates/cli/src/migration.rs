//! Local schema-migration artifact validation and command orchestration.
//!
//! The CLI owns the filesystem package and local chain invariants.  Router owns the durable
//! applied ledger and execution.  This module deliberately keeps those responsibilities behind
//! [`MigrationTransport`] so local discovery and planning remain testable without importing Router
//! internals.

use candid::{Decode, Encode, IDLArgs, IDLValue, Principal};
use clap::Args;
use gleaph_gql::ast::{GraphTypeSpec, Statement};
use gleaph_gql::parser::parse;
use gleaph_gql::token::Token;
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationArgsV1, ApplySchemaMigrationResult,
    ListSchemaMigrationsArgs, ListSchemaMigrationsArgsV1, ListSchemaMigrationsResult,
    MAX_SCHEMA_MIGRATION_ID_BYTES, MAX_SCHEMA_MIGRATION_STATEMENT_BYTES, MAX_SCHEMA_MIGRATIONS,
    SchemaMigrationApplyStatus, SchemaMigrationChecksum, SchemaMigrationRecord,
    SchemaMigrationStatementProfile, parse_schema_migration_id, schema_migration_checksum,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const DEFAULT_IC_URL: &str = "https://icp-api.io";
const DEFAULT_LOCAL_URL: &str = "http://localhost:8000";

const MANIFEST_FILE: &str = "migration.toml";
const GQL_FILE: &str = "up.gql";
const TEMP_PREFIX: &str = ".gleaph-tmp-";
const ID_PREFIX_WIDTH: usize = 6;

/// Identity captured from a path's lstat result before opening the file.
///
/// On Unix, device/inode pairs remain stable for an opened file even when an attacker replaces
/// the directory entry. Other platforms still get the regular-file and symlink checks, but do not
/// expose a portable identity tuple through the standard library.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

/// File metadata that must remain stable for the duration of one read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileSnapshot {
    identity: FileIdentity,
    length: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanos: i64,
    #[cfg(not(unix))]
    modified: SystemTime,
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        FileIdentity {}
    }
}

fn file_snapshot(metadata: &fs::Metadata) -> Result<FileSnapshot, MigrationError> {
    #[cfg(unix)]
    {
        Ok(FileSnapshot {
            identity: file_identity(metadata),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(FileSnapshot {
            identity: file_identity(metadata),
            length: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}

/// CLI options shared by migration subcommands.
#[derive(Args, Clone, Debug)]
pub struct MigrationDirArgs {
    /// Migration directory. Defaults to `./migrations`.
    #[arg(long, value_name = "PATH", default_value = "migrations")]
    pub dir: PathBuf,
}

/// Strict v1 migration manifest.  Descriptions are human metadata and are intentionally excluded
/// from the execution checksum.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationManifest {
    /// Manifest format version.
    pub format_version: u32,
    /// Canonical migration id; must equal the enclosing directory basename.
    pub id: String,
    /// Parent id.  The unique chain root omits this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Human-readable rationale; not part of the execution identity.
    pub description: String,
}

/// A fully validated immutable migration package loaded from one directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationArtifact {
    /// Parsed manifest metadata.
    pub manifest: MigrationManifest,
    /// Exact UTF-8 payload read from `up.gql`.
    pub gql: String,
    /// Validated typed execution profile.
    pub profile: SchemaMigrationStatementProfile,
    /// Directory containing the two artifact files.
    pub path: PathBuf,
    checksum: SchemaMigrationChecksum,
}

impl MigrationArtifact {
    /// Return the canonical migration id.
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    /// Return the parent id, if this is not the chain root.
    pub fn parent(&self) -> Option<&str> {
        self.manifest.parent.as_deref()
    }

    /// Return the immutable Router wire checksum.
    pub fn checksum(&self) -> &SchemaMigrationChecksum {
        &self.checksum
    }

    /// Return the lowercase hexadecimal checksum digest.
    pub fn checksum_hex(&self) -> String {
        hex_digest(&self.checksum.digest)
    }

    /// Convert the local artifact to the Router apply envelope.
    pub fn apply_args(&self) -> ApplySchemaMigrationArgs {
        ApplySchemaMigrationArgs::V1(ApplySchemaMigrationArgsV1 {
            id: self.manifest.id.clone(),
            parent: self.manifest.parent.clone(),
            checksum: self.checksum.clone(),
            statement: self.gql.clone(),
        })
    }
}

/// A validated linear migration plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationPlan {
    /// Migrations in parent-to-child execution order.
    pub migrations: Vec<MigrationArtifact>,
}

/// Remote status computed by comparing local plan and Router ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationStatus {
    /// Number of local migrations already applied remotely.
    pub applied_count: usize,
    /// Total number of migrations in the local chain.
    pub total_count: usize,
}

/// Remote Router transport required by `status` and `apply`.
///
/// The CLI does not own IC identity, endpoint, or Candid agent construction.  The
/// [`RouterMigrationTransport`] adapter implements these methods with the Router
/// `list_schema_migrations` and `apply_schema_migration` APIs, while tests can use a deterministic
/// fake transport.
pub trait MigrationTransport {
    /// List the durable Router migration ledger in canonical order.
    fn list_schema_migrations(
        &mut self,
        args: ListSchemaMigrationsArgs,
    ) -> Result<ListSchemaMigrationsResult, String>;

    /// Apply one validated migration to Router.
    fn apply_schema_migration(
        &mut self,
        args: ApplySchemaMigrationArgs,
    ) -> Result<ApplySchemaMigrationResult, String>;
}

/// IC-agent transport for the Router schema-migration methods.
///
/// This adapter owns endpoint and identity setup only.  Migration validation, ordering, checksum
/// comparison, and preflight remain in the pure functions above.
pub struct RouterMigrationTransport {
    agent: ic_agent::Agent,
    canister: Principal,
    runtime: tokio::runtime::Runtime,
}

impl RouterMigrationTransport {
    /// Build a transport using the same network and identity conventions as codegen.
    pub fn connect(
        canister: &str,
        network: &str,
        identity: Option<&Path>,
        fetch_root_key: bool,
    ) -> Result<Self, MigrationError> {
        let (url, should_fetch_root_key) = resolve_network(network, fetch_root_key)?;
        let canister = Principal::from_text(canister).map_err(|error| {
            MigrationError::Remote(format!("invalid canister principal: {error}"))
        })?;
        let agent = if let Some(identity) = identity {
            let identity = ic_agent::identity::Secp256k1Identity::from_pem_file(identity).map_err(
                |error| {
                    MigrationError::Remote(format!("read identity {}: {error}", identity.display()))
                },
            )?;
            ic_agent::Agent::builder()
                .with_url(url)
                .with_identity(identity)
                .build()
                .map_err(|error| MigrationError::Remote(format!("create IC agent: {error}")))?
        } else {
            ic_agent::Agent::builder()
                .with_url(url)
                .build()
                .map_err(|error| MigrationError::Remote(format!("create IC agent: {error}")))?
        };
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|error| MigrationError::Remote(format!("create async runtime: {error}")))?;
        if should_fetch_root_key {
            runtime
                .block_on(agent.fetch_root_key())
                .map_err(|error| MigrationError::Remote(format!("fetch IC root key: {error}")))?;
        }
        Ok(Self {
            agent,
            canister,
            runtime,
        })
    }

    fn block_on<T>(
        &self,
        future: impl std::future::Future<Output = T>,
    ) -> Result<T, MigrationError> {
        Ok(self.runtime.block_on(future))
    }
}

impl MigrationTransport for RouterMigrationTransport {
    fn list_schema_migrations(
        &mut self,
        args: ListSchemaMigrationsArgs,
    ) -> Result<ListSchemaMigrationsResult, String> {
        let encoded = Encode!(&args)
            .map_err(|error| format!("encode list_schema_migrations args: {error}"))?;
        let response = self
            .block_on(
                self.agent
                    .query(&self.canister, "list_schema_migrations")
                    .with_arg(encoded)
                    .call(),
            )
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("query list_schema_migrations: {error}"))?;
        decode_router_result(&response, "list_schema_migrations")
    }

    fn apply_schema_migration(
        &mut self,
        args: ApplySchemaMigrationArgs,
    ) -> Result<ApplySchemaMigrationResult, String> {
        let encoded = Encode!(&args)
            .map_err(|error| format!("encode apply_schema_migration args: {error}"))?;
        let response = self
            .block_on(
                self.agent
                    .update(&self.canister, "apply_schema_migration")
                    .with_arg(encoded)
                    .call_and_wait(),
            )
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("update apply_schema_migration: {error}"))?;
        decode_router_result(&response, "apply_schema_migration")
    }
}

fn resolve_network(network: &str, fetch_root_key: bool) -> Result<(&str, bool), MigrationError> {
    match network {
        "ic" => Ok((DEFAULT_IC_URL, false)),
        "local" => Ok((DEFAULT_LOCAL_URL, true)),
        url if url.starts_with("http://") || url.starts_with("https://") => {
            if !fetch_root_key {
                return Err(MigrationError::Remote(
                    "a custom network URL requires --fetch-root-key".into(),
                ));
            }
            Ok((url, true))
        }
        other => Err(MigrationError::Remote(format!(
            "unknown network {other:?}; expected \"ic\", \"local\", or an http(s) URL"
        ))),
    }
}

fn decode_router_result<T: candid::CandidType + for<'de> serde::Deserialize<'de>>(
    response: &[u8],
    method: &str,
) -> Result<T, String> {
    let args = IDLArgs::from_bytes(response)
        .map_err(|error| format!("decode {method} response: {error}"))?;
    let Some(IDLValue::Variant(result)) = args.args.first() else {
        return Err(format!("decode {method} response: expected Result variant"));
    };
    let value = &result.0.val;
    if result.0.id.get_id() != candid::idl_hash("Ok") {
        return Err(format!("Router rejected {method}: {value:?}"));
    }
    let payload = IDLArgs::new(std::slice::from_ref(value))
        .to_bytes()
        .map_err(|error| format!("decode {method} payload: {error}"))?;
    Decode!(&payload, T).map_err(|error| format!("decode {method} payload: {error}"))
}

/// Errors raised while loading, validating, planning, or publishing artifacts.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MigrationError {
    /// Filesystem or UTF-8 error.
    #[error("{0}")]
    Io(String),
    /// Manifest TOML was invalid or violated the v1 shape.
    #[error("invalid migration manifest: {0}")]
    Manifest(String),
    /// Migration id violated the canonical grammar.
    #[error("invalid migration id {0:?}; expected six digits, underscore, and a lowercase slug")]
    InvalidId(String),
    /// Directory name and manifest id diverged.
    #[error("migration directory {directory:?} does not match manifest id {manifest:?}")]
    DirectoryId { directory: String, manifest: String },
    /// Directory contains an unexpected entry or symlink.
    #[error("invalid migration directory {path}: {reason}")]
    InvalidDirectory { path: String, reason: String },
    /// GQL payload violated the additive migration dialect.
    #[error("invalid migration GQL: {0}")]
    Gql(String),
    /// Chain topology was not one root-to-head linear sequence.
    #[error("invalid migration chain: {0}")]
    Chain(String),
    /// Remote Router operation failed.
    #[error("Router migration operation failed: {0}")]
    Remote(String),
}

impl From<io::Error> for MigrationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Discover and validate every immediate migration directory under `root`.
pub fn discover(root: &Path) -> Result<MigrationPlan, MigrationError> {
    let root_meta = fs::symlink_metadata(root).map_err(MigrationError::from)?;
    if !root_meta.is_dir() {
        return Err(MigrationError::InvalidDirectory {
            path: root.display().to_string(),
            reason: "migration root is not a directory".into(),
        });
    }

    let mut artifacts = BTreeMap::new();
    let mut prefixes = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| MigrationError::InvalidDirectory {
                path: entry.path().display().to_string(),
                reason: "entry name is not valid UTF-8".into(),
            })?;
        if name.starts_with(TEMP_PREFIX) {
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err(MigrationError::InvalidDirectory {
                    path: entry.path().display().to_string(),
                    reason: "temporary entry must not be a symlink".into(),
                });
            }
            if metadata.is_dir() {
                continue;
            }
            return Err(MigrationError::InvalidDirectory {
                path: entry.path().display().to_string(),
                reason: "temporary entries must be directories".into(),
            });
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(MigrationError::InvalidDirectory {
                path: entry.path().display().to_string(),
                reason: "symlinks are not allowed".into(),
            });
        }
        if !metadata.is_dir() {
            return Err(MigrationError::InvalidDirectory {
                path: entry.path().display().to_string(),
                reason: "migration root may contain only migration directories".into(),
            });
        }
        let artifact = load_artifact(&entry.path())?;
        let prefix = numeric_prefix(artifact.id())?;
        if !prefixes.insert(prefix) {
            return Err(MigrationError::Chain(format!(
                "numeric migration prefix {prefix:06} is duplicated"
            )));
        }
        if artifacts
            .insert(artifact.id().to_owned(), artifact)
            .is_some()
        {
            return Err(MigrationError::Chain(format!(
                "duplicate migration id {name:?}"
            )));
        }
    }

    order_chain(artifacts)
}

/// Validate a migration directory and load its two immutable files.
pub fn load_artifact(path: &Path) -> Result<MigrationArtifact, MigrationError> {
    let dir_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| MigrationError::InvalidDirectory {
            path: path.display().to_string(),
            reason: "directory name is not valid UTF-8".into(),
        })?;
    validate_id(dir_name)?;
    load_artifact_named(path, dir_name)
}

fn load_artifact_named(
    path: &Path,
    expected_dir_name: &str,
) -> Result<MigrationArtifact, MigrationError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(MigrationError::InvalidDirectory {
            path: path.display().to_string(),
            reason: "migration package must be a regular directory".into(),
        });
    }

    let mut names = BTreeSet::new();
    let mut file_snapshots = BTreeMap::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| MigrationError::InvalidDirectory {
                path: entry.path().display().to_string(),
                reason: "entry name is not valid UTF-8".into(),
            })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(MigrationError::InvalidDirectory {
                path: entry.path().display().to_string(),
                reason: "symlinks are not allowed".into(),
            });
        }
        if !metadata.is_file() || !names.insert(name.to_owned()) {
            return Err(MigrationError::InvalidDirectory {
                path: entry.path().display().to_string(),
                reason: "package must contain exactly two regular files".into(),
            });
        }
        file_snapshots.insert(name.to_owned(), file_snapshot(&metadata)?);
        if name != MANIFEST_FILE && name != GQL_FILE {
            return Err(MigrationError::InvalidDirectory {
                path: entry.path().display().to_string(),
                reason: format!("unexpected file {name:?}"),
            });
        }
    }
    let expected = BTreeSet::from([MANIFEST_FILE.to_owned(), GQL_FILE.to_owned()]);
    if names != expected {
        return Err(MigrationError::InvalidDirectory {
            path: path.display().to_string(),
            reason: "package must contain exactly migration.toml and up.gql".into(),
        });
    }

    let manifest_bytes = read_text_file_with_identity(
        &path.join(MANIFEST_FILE),
        false,
        file_snapshots.get(MANIFEST_FILE).copied(),
        None,
    )?;
    let manifest: MigrationManifest = toml::from_str(&manifest_bytes)
        .map_err(|error| MigrationError::Manifest(error.to_string()))?;
    if manifest.format_version != 1 {
        return Err(MigrationError::Manifest(format!(
            "unsupported format_version {}; expected 1",
            manifest.format_version
        )));
    }
    validate_id(&manifest.id)?;
    if manifest.id != expected_dir_name {
        return Err(MigrationError::DirectoryId {
            directory: expected_dir_name.to_owned(),
            manifest: manifest.id,
        });
    }
    if let Some(parent) = &manifest.parent {
        validate_id(parent)?;
        if parent == &manifest.id {
            return Err(MigrationError::Chain(
                "a migration cannot parent itself".into(),
            ));
        }
    }

    let gql = read_text_file_with_identity(
        &path.join(GQL_FILE),
        true,
        file_snapshots.get(GQL_FILE).copied(),
        Some(MAX_SCHEMA_MIGRATION_STATEMENT_BYTES),
    )?;
    let profile = validate_gql(&gql)?;
    let checksum = calculate_checksum(&manifest, gql.as_bytes());
    Ok(MigrationArtifact {
        manifest,
        gql,
        profile,
        path: path.to_path_buf(),
        checksum,
    })
}

/// Build a local plan without performing any Router calls or writes.
pub fn plan(root: &Path) -> Result<MigrationPlan, MigrationError> {
    discover(root)
}

/// Query Router's durable migration ledger and compare it with a local plan.
pub fn status<T: MigrationTransport>(
    root: &Path,
    transport: &mut T,
) -> Result<MigrationStatus, MigrationError> {
    let local = discover(root)?;
    status_for_plan(&local, transport)
}

fn status_for_plan<T: MigrationTransport>(
    local: &MigrationPlan,
    transport: &mut T,
) -> Result<MigrationStatus, MigrationError> {
    let mut remote_count = 0usize;
    let mut start_after = None;
    let mut visited_cursors = BTreeSet::new();
    loop {
        if !visited_cursors.insert(start_after.clone()) {
            return Err(MigrationError::Remote(format!(
                "Router pagination cursor repeated: {start_after:?}"
            )));
        }
        let result = transport
            .list_schema_migrations(ListSchemaMigrationsArgs::V1(ListSchemaMigrationsArgsV1 {
                start_after: start_after.clone(),
                limit: gleaph_migration_api::MAX_SCHEMA_MIGRATION_LIST_LIMIT,
            }))
            .map_err(MigrationError::Remote)?;
        let ListSchemaMigrationsResult::V1(result) = result;
        let page_len = result.migrations.len();
        if page_len > gleaph_migration_api::MAX_SCHEMA_MIGRATION_LIST_LIMIT as usize {
            return Err(MigrationError::Remote(format!(
                "Router returned an oversized migration page: {page_len} records (maximum {})",
                gleaph_migration_api::MAX_SCHEMA_MIGRATION_LIST_LIMIT
            )));
        }
        if page_len > MAX_SCHEMA_MIGRATIONS.saturating_sub(remote_count) {
            return Err(MigrationError::Remote(format!(
                "Router returned more than {MAX_SCHEMA_MIGRATIONS} migration records"
            )));
        }
        if let Some(next) = result.next_start_after.as_deref() {
            let Some(last) = result.migrations.last() else {
                return Err(MigrationError::Remote(
                    "Router returned a next_start_after cursor without records".into(),
                ));
            };
            let last_id = record_id(last);
            if next != last_id {
                return Err(MigrationError::Remote(format!(
                    "Router returned next_start_after {next:?}, but the last migration id was {last_id:?}"
                )));
            }
        }
        for record in result.migrations {
            if remote_count >= local.migrations.len() {
                return Err(MigrationError::Chain(
                    "Router has migrations absent from the local chain".into(),
                ));
            }
            let expected = &local.migrations[remote_count];
            let SchemaMigrationRecord::V1(record) = record;
            if record.id != expected.id() {
                return Err(MigrationError::Chain(format!(
                    "Router migration {:?} is not the local prefix migration {:?}",
                    record.id,
                    expected.id()
                )));
            }
            if record.parent.as_deref() != expected.parent() {
                return Err(MigrationError::Chain(format!(
                    "parent mismatch for applied migration {:?}",
                    record.id
                )));
            }
            if record.checksum != *expected.checksum() {
                return Err(MigrationError::Chain(format!(
                    "checksum mismatch for applied migration {:?}",
                    record.id
                )));
            }
            remote_count += 1;
        }
        match result.next_start_after {
            Some(next) => {
                start_after = Some(next);
            }
            _ => break,
        }
    }
    Ok(MigrationStatus {
        applied_count: remote_count,
        total_count: local.migrations.len(),
    })
}

/// Apply every pending local migration after a complete local/remote preflight.
pub fn apply<T: MigrationTransport>(
    root: &Path,
    transport: &mut T,
) -> Result<Vec<SchemaMigrationApplyStatus>, MigrationError> {
    let local = discover(root)?;
    let current = status_for_plan(&local, transport)?;
    let mut outcomes = Vec::new();
    for (index, artifact) in local
        .migrations
        .iter()
        .enumerate()
        .skip(current.applied_count)
    {
        let result = match transport.apply_schema_migration(artifact.apply_args()) {
            Ok(result) => result,
            Err(original) => {
                // An ingress timeout is ambiguous: the Router may have committed before the
                // transport failed. Re-read the canonical ledger and advance only on an exact
                // id/checksum match; never infer success from the update error alone.
                match status_for_plan(&local, transport) {
                    Ok(recovered) if recovered.applied_count > index => {
                        outcomes.push(SchemaMigrationApplyStatus::Replay);
                        continue;
                    }
                    Ok(_) => {
                        return Err(MigrationError::Remote(format!(
                            "{original}; Router ledger does not contain the migration after recovery"
                        )));
                    }
                    Err(recovery) => {
                        return Err(MigrationError::Remote(format!(
                            "{original}; recovery status failed: {recovery}"
                        )));
                    }
                }
            }
        };
        let ApplySchemaMigrationResult::V1(result) = result;
        if record_id(&result.record) != artifact.id() {
            return Err(MigrationError::Remote(format!(
                "Router returned record for {:?} while applying {:?}",
                record_id(&result.record),
                artifact.id()
            )));
        }
        let SchemaMigrationRecord::V1(record) = &result.record;
        if record.parent.as_deref() != artifact.parent()
            || record.checksum != *artifact.checksum()
            || record.statement != artifact.gql
            || record.profile != artifact.profile
        {
            return Err(MigrationError::Remote(format!(
                "Router returned a mismatched record for {:?}",
                artifact.id()
            )));
        }
        outcomes.push(result.status);
    }
    Ok(outcomes)
}

/// Publish a new migration package via a same-filesystem temporary directory and rename.
pub fn create_new(
    root: &Path,
    slug: &str,
    description: &str,
    gql: Option<&Path>,
) -> Result<MigrationArtifact, MigrationError> {
    validate_slug(slug)?;
    let existing = match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(MigrationError::InvalidDirectory {
                path: root.display().to_string(),
                reason: "migration root must not be a symlink".into(),
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(MigrationError::InvalidDirectory {
                path: root.display().to_string(),
                reason: "migration root is not a directory".into(),
            });
        }
        Ok(_) => discover(root)?,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
            MigrationPlan {
                migrations: Vec::new(),
            }
        }
        Err(error) => return Err(MigrationError::Io(error.to_string())),
    };
    let next_prefix = existing
        .migrations
        .iter()
        .map(|migration| numeric_prefix(migration.id()).expect("discovery validates ids"))
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| MigrationError::Chain("migration id prefix exhausted".into()))?;
    if next_prefix > 999_999 {
        return Err(MigrationError::Chain(
            "migration id prefix exhausted".into(),
        ));
    }
    let id = format!("{next_prefix:06}_{slug}");
    let parent = existing
        .migrations
        .last()
        .map(|migration| migration.id().to_owned());
    let manifest = MigrationManifest {
        format_version: 1,
        id: id.clone(),
        parent,
        description: description.to_owned(),
    };
    let gql_bytes = if let Some(source) = gql {
        read_file_bytes_checked(source, None, Some(MAX_SCHEMA_MIGRATION_STATEMENT_BYTES))?
    } else {
        format!("CREATE GRAPH TYPE {slug} {{}}\n").into_bytes()
    };
    let gql = String::from_utf8(gql_bytes)
        .map_err(|error| MigrationError::Io(format!("up.gql is not UTF-8: {error}")))?;

    let temporary = temporary_path(root);
    fs::create_dir(&temporary)?;
    let result = (|| {
        let manifest_text = toml::to_string(&manifest)
            .map_err(|e| MigrationError::Manifest(format!("serialize migration manifest: {e}")))?;
        write_new_file(&temporary.join(MANIFEST_FILE), manifest_text.as_bytes())?;
        write_new_file(&temporary.join(GQL_FILE), gql.as_bytes())?;
        let mut artifact = load_artifact_named(&temporary, &id)?;
        let final_path = root.join(&id);
        if final_path.exists() {
            return Err(MigrationError::Chain(format!(
                "migration directory {id:?} already exists"
            )));
        }
        fs::rename(&temporary, &final_path)?;
        artifact.path = final_path;
        Ok(artifact)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

fn order_chain(
    artifacts: BTreeMap<String, MigrationArtifact>,
) -> Result<MigrationPlan, MigrationError> {
    if artifacts.is_empty() {
        return Ok(MigrationPlan {
            migrations: Vec::new(),
        });
    }
    let roots: Vec<_> = artifacts
        .values()
        .filter(|artifact| artifact.parent().is_none())
        .map(|artifact| artifact.id().to_owned())
        .collect();
    if roots.len() != 1 {
        return Err(MigrationError::Chain(format!(
            "expected exactly one root, found {}",
            roots.len()
        )));
    }
    let mut children: BTreeMap<&str, &str> = BTreeMap::new();
    for artifact in artifacts.values() {
        let Some(parent) = artifact.parent() else {
            continue;
        };
        let Some(parent_artifact) = artifacts.get(parent) else {
            return Err(MigrationError::Chain(format!(
                "migration {:?} refers to missing parent {:?}",
                artifact.id(),
                parent
            )));
        };
        let parent_prefix = numeric_prefix(parent_artifact.id())?;
        let child_prefix = numeric_prefix(artifact.id())?;
        if child_prefix <= parent_prefix {
            return Err(MigrationError::Chain(format!(
                "migration {:?} must have a greater numeric prefix than parent {:?}",
                artifact.id(),
                parent
            )));
        }
        if children.insert(parent, artifact.id()).is_some() {
            return Err(MigrationError::Chain(format!(
                "migration parent {:?} has more than one child",
                parent
            )));
        }
    }
    let mut ordered = Vec::with_capacity(artifacts.len());
    let mut current = roots[0].as_str();
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(current.to_owned()) {
            return Err(MigrationError::Chain(
                "migration parent links contain a cycle".into(),
            ));
        }
        ordered.push(
            artifacts
                .get(current)
                .expect("chain entry was checked above")
                .clone(),
        );
        let Some(next) = children.get(current) else {
            break;
        };
        current = next;
    }
    if ordered.len() != artifacts.len() {
        return Err(MigrationError::Chain(
            "migration graph contains disconnected records".into(),
        ));
    }
    Ok(MigrationPlan {
        migrations: ordered,
    })
}

/// Read one regular file through an opened handle while checking that its directory entry was not
/// replaced during the read.
///
/// The expected identity is captured while enumerating a migration package. On Unix the device and
/// inode pair is checked against both the path's lstat result and the opened handle before and
/// after reading. The standard library does not expose an equivalent portable identity tuple on
/// every platform, so non-Unix builds retain the regular-file and non-symlink checks only. Ancestor
/// directories are still traversed through path-based `read_dir`; pinning those directories would
/// require platform-specific *at/openat-style APIs or an additional filesystem-capability crate.
fn read_file_bytes_checked(
    path: &Path,
    expected_snapshot: Option<FileSnapshot>,
    max_bytes: Option<usize>,
) -> Result<Vec<u8>, MigrationError> {
    let path_snapshot = regular_file_snapshot(path)?;
    let expected_snapshot = expected_snapshot.unwrap_or(path_snapshot);
    if path_snapshot != expected_snapshot {
        return Err(file_changed_error(path));
    }

    let mut file = OpenOptions::new().read(true).open(path)?;
    let before = file.metadata()?;
    let before_snapshot = verify_opened_file(path, &before, expected_snapshot)?;
    verify_path_snapshot(path, expected_snapshot)?;

    if let Some(max_bytes) = max_bytes
        && before.len() > max_bytes as u64
    {
        return Err(statement_too_large_error(path, max_bytes));
    }

    let mut bytes = Vec::new();
    match max_bytes {
        Some(max_bytes) => {
            // Read one byte beyond the limit so a file that grows after the preflight metadata
            // check is rejected without allocating an unbounded buffer.
            let read_limit = (max_bytes as u64).saturating_add(1);
            (&mut file).take(read_limit).read_to_end(&mut bytes)?;
            if bytes.len() > max_bytes {
                return Err(statement_too_large_error(path, max_bytes));
            }
        }
        None => {
            file.read_to_end(&mut bytes)?;
        }
    }

    let after = file.metadata()?;
    let after_snapshot = verify_opened_file(path, &after, expected_snapshot)?;
    if after_snapshot != before_snapshot {
        return Err(file_changed_error(path));
    }
    verify_path_snapshot(path, expected_snapshot)?;
    Ok(bytes)
}

fn regular_file_snapshot(path: &Path) -> Result<FileSnapshot, MigrationError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(MigrationError::Io(format!(
            "{} must be a regular non-symlink file",
            path.display()
        )));
    }
    file_snapshot(&metadata)
}

fn verify_opened_file(
    path: &Path,
    metadata: &fs::Metadata,
    expected_snapshot: FileSnapshot,
) -> Result<FileSnapshot, MigrationError> {
    if !metadata.is_file() {
        return Err(file_changed_error(path));
    }
    let snapshot = file_snapshot(metadata).map_err(|_| file_changed_error(path))?;
    if snapshot != expected_snapshot {
        return Err(file_changed_error(path));
    }
    Ok(snapshot)
}

fn verify_path_snapshot(
    path: &Path,
    expected_snapshot: FileSnapshot,
) -> Result<(), MigrationError> {
    let actual_snapshot = regular_file_snapshot(path)?;
    if actual_snapshot != expected_snapshot {
        return Err(file_changed_error(path));
    }
    Ok(())
}

fn file_changed_error(path: &Path) -> MigrationError {
    MigrationError::Io(format!("{} changed while being read", path.display()))
}

fn statement_too_large_error(path: &Path, max_bytes: usize) -> MigrationError {
    MigrationError::Io(format!(
        "{} exceeds maximum migration statement size of {max_bytes} bytes",
        path.display()
    ))
}

fn load_text_bytes_with_identity(
    path: &Path,
    require_lf_terminal: bool,
    expected_snapshot: Option<FileSnapshot>,
    max_bytes: Option<usize>,
) -> Result<Vec<u8>, MigrationError> {
    let bytes = read_file_bytes_checked(path, expected_snapshot, max_bytes)?;
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(MigrationError::Io(format!(
            "{} must not contain a UTF-8 BOM",
            path.display()
        )));
    }
    std::str::from_utf8(&bytes)
        .map_err(|error| MigrationError::Io(format!("{} is not UTF-8: {error}", path.display())))?;
    if bytes.contains(&0) {
        return Err(MigrationError::Io(format!(
            "{} contains NUL bytes",
            path.display()
        )));
    }
    if require_lf_terminal {
        if bytes.contains(&b'\r') {
            return Err(MigrationError::Io(format!(
                "{} must use LF line endings",
                path.display()
            )));
        }
        if !bytes.ends_with(b"\n") {
            return Err(MigrationError::Io(format!(
                "{} must end with a newline",
                path.display()
            )));
        }
    }
    Ok(bytes)
}

fn read_text_file_with_identity(
    path: &Path,
    require_lf_terminal: bool,
    expected_snapshot: Option<FileSnapshot>,
    max_bytes: Option<usize>,
) -> Result<String, MigrationError> {
    let bytes =
        load_text_bytes_with_identity(path, require_lf_terminal, expected_snapshot, max_bytes)?;
    String::from_utf8(bytes).map_err(|error| MigrationError::Io(error.to_string()))
}

fn validate_gql(gql: &str) -> Result<SchemaMigrationStatementProfile, MigrationError> {
    let lexical = gleaph_gql::lexer::tokenize_with_comments(gql)
        .map_err(|error| MigrationError::Gql(error.to_string()))?;
    if lexical
        .tokens
        .iter()
        .any(|token| matches!(token.token, Token::Param(_) | Token::SubstitutedParam(_)))
    {
        return Err(MigrationError::Gql(
            "parameters are not allowed in migrations".into(),
        ));
    }
    let program = parse(gql).map_err(|error| MigrationError::Gql(error.to_string()))?;
    gleaph_gql::validate::validate(&program)
        .map_err(|error| MigrationError::Gql(error.to_string()))?;
    if !program.session_activity.is_empty() {
        return Err(MigrationError::Gql(
            "SESSION commands are not allowed in migrations".into(),
        ));
    }
    let activity = program.transaction_activity.ok_or_else(|| {
        MigrationError::Gql("migration must contain one catalog statement".into())
    })?;
    if activity.start.is_some() || activity.end.is_some() {
        return Err(MigrationError::Gql(
            "transaction commands are not allowed in migrations".into(),
        ));
    }
    let body = activity.body.ok_or_else(|| {
        MigrationError::Gql("migration must contain one catalog statement".into())
    })?;
    if !body.next.is_empty() {
        return Err(MigrationError::Gql(
            "NEXT-chained statements are not allowed in migrations".into(),
        ));
    }
    let operation = match body.first {
        Statement::CreateGraphType(create)
            if !create.if_not_exists
                && !create.or_replace
                && create.copy_of.is_none()
                && create.name.parts.len() == 1
                && lexical
                    .tokens
                    .iter()
                    .any(|token| matches!(token.token, Token::LBrace)) =>
        {
            SchemaMigrationStatementProfile::CreateGraphType
        }
        Statement::CreateGraph(create)
            if !create.if_not_exists
                && !create.or_replace
                && create.copy_of.is_none()
                && create.name.parts.len() == 1
                && matches!(
                    &create.graph_type,
                    Some(GraphTypeSpec::Typed {
                        name,
                        typed_keyword: true,
                    }) if name.parts.len() == 1
                ) =>
        {
            let Some(GraphTypeSpec::Typed { .. }) = create.graph_type else {
                unreachable!("guard proves typed graph type")
            };
            SchemaMigrationStatementProfile::CreateTypedGraph
        }
        Statement::CreateGraphType(_) => {
            return Err(MigrationError::Gql(
                "CREATE GRAPH TYPE migrations require one explicit body and forbid IF NOT EXISTS, OR REPLACE, and COPY".into(),
            ));
        }
        Statement::CreateGraph(_) => {
            return Err(MigrationError::Gql(
                "CREATE GRAPH migrations require a simple literal name and TYPED literal type"
                    .into(),
            ));
        }
        _ => {
            return Err(MigrationError::Gql(
                "only additive CREATE GRAPH TYPE or CREATE GRAPH ... TYPED ... is allowed".into(),
            ));
        }
    };
    Ok(operation)
}

fn calculate_checksum(manifest: &MigrationManifest, statement: &[u8]) -> SchemaMigrationChecksum {
    schema_migration_checksum(&manifest.id, manifest.parent.as_deref(), statement)
}

fn record_id(record: &SchemaMigrationRecord) -> &str {
    match record {
        SchemaMigrationRecord::V1(record) => &record.id,
    }
}

fn validate_id(id: &str) -> Result<(), MigrationError> {
    parse_schema_migration_id(id)
        .map(|_| ())
        .ok_or_else(|| MigrationError::InvalidId(id.to_owned()))
}

fn validate_slug(slug: &str) -> Result<(), MigrationError> {
    let valid = !slug.is_empty()
        && slug.split('_').enumerate().all(|(index, segment)| {
            !segment.is_empty()
                && (index > 0 || segment.as_bytes()[0].is_ascii_lowercase())
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
    if slug.len() + ID_PREFIX_WIDTH + 1 > MAX_SCHEMA_MIGRATION_ID_BYTES || !valid {
        return Err(MigrationError::InvalidId(slug.to_owned()));
    }
    Ok(())
}

fn numeric_prefix(id: &str) -> Result<u32, MigrationError> {
    parse_schema_migration_id(id).ok_or_else(|| MigrationError::InvalidId(id.to_owned()))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn temporary_path(root: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    root.join(format!("{TEMP_PREFIX}{}-{nonce}", std::process::id()))
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), MigrationError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_migration_api::MAX_SCHEMA_MIGRATION_LIST_LIMIT;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gleaph-migration-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temporary root");
        path
    }

    fn write_package(root: &Path, id: &str, parent: Option<&str>, gql: &str, description: &str) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).expect("package directory");
        let parent = parent.map(|parent| format!("parent = {parent:?}\n"));
        let manifest = format!(
            "format_version = 1\nid = {id:?}\n{}description = {description:?}\n",
            parent.unwrap_or_default()
        );
        fs::write(dir.join(MANIFEST_FILE), manifest).expect("manifest");
        fs::write(dir.join(GQL_FILE), gql).expect("gql");
    }

    struct FakeMigrationTransport {
        pages: VecDeque<Result<ListSchemaMigrationsResult, String>>,
        calls: Vec<ListSchemaMigrationsArgsV1>,
        apply_results: VecDeque<Result<ApplySchemaMigrationResult, String>>,
        apply_calls: Vec<ApplySchemaMigrationArgsV1>,
    }

    impl FakeMigrationTransport {
        fn new(pages: Vec<Result<ListSchemaMigrationsResult, String>>) -> Self {
            Self {
                pages: pages.into(),
                calls: Vec::new(),
                apply_results: VecDeque::new(),
                apply_calls: Vec::new(),
            }
        }

        fn with_apply_results(
            mut self,
            results: Vec<Result<ApplySchemaMigrationResult, String>>,
        ) -> Self {
            self.apply_results = results.into();
            self
        }
    }

    impl MigrationTransport for FakeMigrationTransport {
        fn list_schema_migrations(
            &mut self,
            args: ListSchemaMigrationsArgs,
        ) -> Result<ListSchemaMigrationsResult, String> {
            let ListSchemaMigrationsArgs::V1(args) = args;
            self.calls.push(args);
            self.pages
                .pop_front()
                .expect("fake transport has a response for every page request")
        }

        fn apply_schema_migration(
            &mut self,
            args: ApplySchemaMigrationArgs,
        ) -> Result<ApplySchemaMigrationResult, String> {
            let ApplySchemaMigrationArgs::V1(args) = args;
            self.apply_calls.push(args);
            self.apply_results
                .pop_front()
                .expect("fake transport has a response for every apply request")
        }
    }

    fn test_artifacts(count: usize) -> Vec<MigrationArtifact> {
        let mut parent = None;
        (0..count)
            .map(|index| {
                let id = format!("{:06}_migration", index + 1);
                let manifest = MigrationManifest {
                    format_version: 1,
                    id: id.clone(),
                    parent: parent.clone(),
                    description: String::new(),
                };
                let gql = format!("CREATE GRAPH TYPE type{index} {{}}\n");
                let artifact = MigrationArtifact {
                    profile: SchemaMigrationStatementProfile::CreateGraphType,
                    checksum: calculate_checksum(&manifest, gql.as_bytes()),
                    manifest,
                    gql,
                    path: PathBuf::new(),
                };
                parent = Some(id);
                artifact
            })
            .collect()
    }

    fn test_record(artifact: &MigrationArtifact) -> SchemaMigrationRecord {
        SchemaMigrationRecord::V1(gleaph_migration_api::SchemaMigrationRecordV1 {
            id: artifact.id().to_owned(),
            parent: artifact.manifest.parent.clone(),
            checksum: artifact.checksum.clone(),
            actor: Principal::anonymous(),
            applied_at: 0,
            statement: artifact.gql.clone(),
            profile: artifact.profile.clone(),
        })
    }

    fn test_page(
        migrations: Vec<SchemaMigrationRecord>,
        next_start_after: Option<String>,
    ) -> Result<ListSchemaMigrationsResult, String> {
        Ok(ListSchemaMigrationsResult::V1(
            gleaph_migration_api::ListSchemaMigrationsResultV1 {
                migrations,
                next_start_after,
            },
        ))
    }

    fn test_apply_result(
        artifact: &MigrationArtifact,
        status: SchemaMigrationApplyStatus,
    ) -> Result<ApplySchemaMigrationResult, String> {
        Ok(ApplySchemaMigrationResult::V1(
            gleaph_migration_api::ApplySchemaMigrationResultV1 {
                status,
                record: test_record(artifact),
            },
        ))
    }

    #[test]
    fn validates_additive_graph_type_and_binds_exact_statement_bytes() {
        let root = temp_root();
        write_package(
            &root,
            "000001_init",
            None,
            "// one\nCREATE GRAPH TYPE Social { NODE Person }\n",
            "first",
        );
        let first = discover(&root).expect("valid package");
        let digest = first.migrations[0].checksum_hex();
        fs::write(
            root.join("000001_init").join(MANIFEST_FILE),
            "format_version = 1\nid = \"000001_init\"\ndescription = \"changed\"\n",
        )
        .expect("manifest rewrite");
        let description_only = discover(&root).expect("description is metadata only");
        assert_eq!(digest, description_only.migrations[0].checksum_hex());
        fs::write(
            root.join("000001_init").join(GQL_FILE),
            "// two\nCREATE GRAPH TYPE Social { NODE Person }\n",
        )
        .expect("gql rewrite");
        let second = discover(&root).expect("same execution");
        assert_ne!(digest, second.migrations[0].checksum_hex());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn validates_the_migration_statement_allowlist() {
        let accepted = [
            (
                "CREATE GRAPH TYPE Social {}\n",
                SchemaMigrationStatementProfile::CreateGraphType,
            ),
            (
                "CREATE GRAPH social TYPED Social\n",
                SchemaMigrationStatementProfile::CreateTypedGraph,
            ),
        ];
        for (statement, expected) in accepted {
            assert_eq!(
                validate_gql(statement).expect("allowlisted statement"),
                expected
            );
        }

        let rejected = [
            "INSERT (n)\n",
            "CREATE GRAPH TYPE Social {} NEXT INSERT (n)\n",
            "CREATE OR REPLACE GRAPH TYPE Social {}\n",
            "CREATE GRAPH TYPE IF NOT EXISTS Social {}\n",
            "SESSION SET VALUE $x :: STRING = 'x'\n",
            "START TRANSACTION\nCREATE GRAPH TYPE Social {}\n",
        ];
        for statement in rejected {
            assert!(
                validate_gql(statement).is_err(),
                "statement should be rejected: {statement:?}"
            );
        }
    }

    #[test]
    fn orders_linear_chain_and_rejects_forks() {
        let root = temp_root();
        write_package(
            &root,
            "000001_init",
            None,
            "CREATE GRAPH TYPE Social {}\n",
            "first",
        );
        write_package(
            &root,
            "000002_a",
            Some("000001_init"),
            "CREATE GRAPH g TYPED Social\n",
            "second",
        );
        let plan = discover(&root).expect("linear chain");
        assert_eq!(plan.migrations.len(), 2);
        write_package(
            &root,
            "000003_b",
            Some("000001_init"),
            "CREATE GRAPH h TYPED Social\n",
            "fork",
        );
        let error = discover(&root).expect_err("fork must be rejected");
        assert!(error.to_string().contains("more than one child"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn new_publishes_two_files_and_derives_parent() {
        let root = temp_root();
        let first = create_new(&root, "init", "first", None).expect("first migration");
        assert_eq!(first.id(), "000001_init");
        let second = create_new(&root, "bind", "second", None).expect("second migration");
        assert_eq!(second.id(), "000002_bind");
        assert_eq!(second.parent(), Some("000001_init"));
        assert_eq!(second.path, root.join("000002_bind"));
        let names: BTreeSet<_> = fs::read_dir(second.path)
            .expect("package files")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            BTreeSet::from([MANIFEST_FILE.into(), GQL_FILE.into()])
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn checked_read_rejects_replaced_regular_file() {
        let root = temp_root();
        let path = root.join(GQL_FILE);
        fs::write(&path, b"original\n").expect("original file");
        let expected = file_snapshot(&fs::symlink_metadata(&path).expect("original metadata"))
            .expect("original snapshot");

        let replacement = root.join("replacement.gql");
        fs::write(&replacement, b"replacement\n").expect("replacement file");
        fs::rename(replacement, &path).expect("replace file atomically");

        let error = read_text_file_with_identity(&path, false, Some(expected), None)
            .expect_err("replacement must fail the identity check");
        assert!(error.to_string().contains("changed while being read"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn checked_read_rejects_replaced_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let path = root.join(GQL_FILE);
        fs::write(&path, b"original\n").expect("original file");
        let expected = file_snapshot(&fs::symlink_metadata(&path).expect("original metadata"))
            .expect("original snapshot");
        let target = root.join("outside.gql");
        fs::write(&target, b"outside\n").expect("outside file");
        fs::remove_file(&path).expect("remove original file");
        symlink(&target, &path).expect("replace with symlink");

        let error = read_text_file_with_identity(&path, false, Some(expected), None)
            .expect_err("symlink replacement must fail the lstat check");
        assert!(error.to_string().contains("regular non-symlink"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejects_oversized_statement_before_gql_parse() {
        let root = temp_root();
        let oversized = "x".repeat(MAX_SCHEMA_MIGRATION_STATEMENT_BYTES + 1);
        write_package(&root, "000001_oversized", None, &oversized, "too large");

        let error = discover(&root).expect_err("oversized statement must be rejected");
        assert!(
            error
                .to_string()
                .contains("exceeds maximum migration statement size")
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn create_new_rejects_symlinked_up_source() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let target = root.join("source.gql");
        fs::write(&target, b"CREATE GRAPH TYPE Social {}\n").expect("source file");
        let source = root.join("source-link.gql");
        symlink(&target, &source).expect("source symlink");

        let error = create_new(&root.join("migrations"), "init", "first", Some(&source))
            .expect_err("symlinked source must fail the lstat check");
        assert!(error.to_string().contains("regular non-symlink"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn apply_dispatches_only_pending_migrations_in_parent_order() {
        let root = temp_root();
        write_package(
            &root,
            "000001_init",
            None,
            "CREATE GRAPH TYPE Social {}\n",
            "first",
        );
        write_package(
            &root,
            "000002_bind",
            Some("000001_init"),
            "CREATE GRAPH social TYPED Social\n",
            "second",
        );
        write_package(
            &root,
            "000003_more",
            Some("000002_bind"),
            "CREATE GRAPH other TYPED Social\n",
            "third",
        );
        let plan = discover(&root).expect("test chain");
        let initial = test_page(vec![test_record(&plan.migrations[0])], None);
        let mut transport = FakeMigrationTransport::new(vec![initial]).with_apply_results(vec![
            test_apply_result(&plan.migrations[1], SchemaMigrationApplyStatus::Applied),
            test_apply_result(&plan.migrations[2], SchemaMigrationApplyStatus::Applied),
        ]);

        let outcomes = apply(&root, &mut transport).expect("pending migrations apply");

        assert_eq!(outcomes, vec![SchemaMigrationApplyStatus::Applied; 2]);
        assert_eq!(
            transport
                .apply_calls
                .iter()
                .map(|args| args.id.as_str())
                .collect::<Vec<_>>(),
            vec!["000002_bind", "000003_more"]
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn apply_rejects_remote_drift_before_any_apply_call() {
        let root = temp_root();
        write_package(
            &root,
            "000001_init",
            None,
            "CREATE GRAPH TYPE Social {}\n",
            "first",
        );
        let plan = discover(&root).expect("test chain");
        let mut drifted = test_record(&plan.migrations[0]);
        let SchemaMigrationRecord::V1(record) = &mut drifted;
        record.checksum.digest[0] ^= 1;
        let mut transport = FakeMigrationTransport::new(vec![test_page(vec![drifted], None)]);

        let error = apply(&root, &mut transport).expect_err("drift must fail closed");

        assert!(
            matches!(error, MigrationError::Chain(message) if message.contains("checksum mismatch"))
        );
        assert!(transport.apply_calls.is_empty());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn apply_rejects_each_mismatched_remote_record_field() {
        for field in ["id", "parent", "checksum", "statement", "profile"] {
            let root = temp_root();
            write_package(
                &root,
                "000001_init",
                None,
                "CREATE GRAPH TYPE Social {}\n",
                "first",
            );
            let plan = discover(&root).expect("test chain");
            let mut response =
                test_apply_result(&plan.migrations[0], SchemaMigrationApplyStatus::Applied)
                    .expect("apply response");
            let ApplySchemaMigrationResult::V1(result) = &mut response;
            let SchemaMigrationRecord::V1(record) = &mut result.record;
            match field {
                "id" => record.id = "000002_wrong".into(),
                "parent" => record.parent = Some("000000_wrong".into()),
                "checksum" => record.checksum.digest[0] ^= 1,
                "statement" => record.statement.push_str(" -- drift"),
                "profile" => record.profile = SchemaMigrationStatementProfile::CreateTypedGraph,
                _ => unreachable!(),
            }
            let mut transport = FakeMigrationTransport::new(vec![test_page(vec![], None)])
                .with_apply_results(vec![Ok(response)]);

            let error = apply(&root, &mut transport).expect_err("mismatched record must fail");

            assert!(matches!(error, MigrationError::Remote(_)), "field={field}");
            assert_eq!(transport.apply_calls.len(), 1, "field={field}");
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn apply_recovers_an_ambiguous_update_only_from_an_exact_ledger_record() {
        let root = temp_root();
        write_package(
            &root,
            "000001_init",
            None,
            "CREATE GRAPH TYPE Social {}\n",
            "first",
        );
        let plan = discover(&root).expect("test chain");
        let mut transport = FakeMigrationTransport::new(vec![
            test_page(vec![], None),
            test_page(vec![test_record(&plan.migrations[0])], None),
        ])
        .with_apply_results(vec![Err("transport timeout".into())]);

        let outcomes = apply(&root, &mut transport).expect("exact replay recovery");

        assert_eq!(outcomes, vec![SchemaMigrationApplyStatus::Replay]);
        assert_eq!(transport.apply_calls.len(), 1);
        assert_eq!(transport.calls.len(), 2);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn apply_reports_an_ambiguous_failure_without_ledger_evidence() {
        let root = temp_root();
        write_package(
            &root,
            "000001_init",
            None,
            "CREATE GRAPH TYPE Social {}\n",
            "first",
        );
        let mut transport =
            FakeMigrationTransport::new(vec![test_page(vec![], None), test_page(vec![], None)])
                .with_apply_results(vec![Err("transport timeout".into())]);

        let error = apply(&root, &mut transport).expect_err("missing ledger evidence");

        assert!(
            matches!(error, MigrationError::Remote(message) if message.contains("transport timeout") && message.contains("does not contain"))
        );
        assert_eq!(transport.calls.len(), 2);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn status_follows_exclusive_cursor_across_multiple_pages() {
        let artifacts = test_artifacts(17);
        let records: Vec<_> = artifacts.iter().map(test_record).collect();
        let first_cursor = artifacts[15].id().to_owned();
        let mut transport = FakeMigrationTransport::new(vec![
            test_page(records[..16].to_vec(), Some(first_cursor.clone())),
            test_page(records[16..].to_vec(), None),
        ]);

        let plan = MigrationPlan {
            migrations: artifacts,
        };
        let status =
            status_for_plan(&plan, &mut transport).expect("valid pages should produce status");

        assert_eq!(status.applied_count, 17);
        assert_eq!(status.total_count, 17);
        assert_eq!(transport.calls.len(), 2);
        assert_eq!(transport.calls[0].start_after, None);
        assert_eq!(transport.calls[1].start_after, Some(first_cursor));
        assert!(
            transport
                .calls
                .iter()
                .all(|args| args.limit == MAX_SCHEMA_MIGRATION_LIST_LIMIT)
        );
    }

    #[test]
    fn status_rejects_a_cyclic_cursor() {
        let mut artifacts = test_artifacts(2);
        artifacts[1].manifest.id = artifacts[0].id().to_owned();
        let records: Vec<_> = artifacts.iter().map(test_record).collect();
        let first_cursor = artifacts[0].id().to_owned();
        let mut transport = FakeMigrationTransport::new(vec![
            test_page(vec![records[0].clone()], Some(first_cursor.clone())),
            test_page(vec![records[1].clone()], Some(first_cursor)),
        ]);

        let plan = MigrationPlan {
            migrations: artifacts,
        };
        let error =
            status_for_plan(&plan, &mut transport).expect_err("a cyclic cursor must fail closed");
        assert!(matches!(error, MigrationError::Remote(message) if message.contains("repeated")));
    }

    #[test]
    fn status_rejects_a_stale_next_cursor() {
        let artifacts = test_artifacts(2);
        let mut transport = FakeMigrationTransport::new(vec![test_page(
            vec![test_record(&artifacts[0])],
            Some(artifacts[1].id().to_owned()),
        )]);

        let plan = MigrationPlan {
            migrations: artifacts,
        };
        let error = status_for_plan(&plan, &mut transport)
            .expect_err("a cursor not equal to the page tail must fail closed");
        assert!(
            matches!(error, MigrationError::Remote(message) if message.contains("last migration id"))
        );
    }

    #[test]
    fn status_rejects_an_oversized_page() {
        let artifact = test_artifacts(1).remove(0);
        let record = test_record(&artifact);
        let mut transport = FakeMigrationTransport::new(vec![test_page(
            vec![record; MAX_SCHEMA_MIGRATION_LIST_LIMIT as usize + 1],
            None,
        )]);

        let plan = MigrationPlan {
            migrations: Vec::new(),
        };
        let error = status_for_plan(&plan, &mut transport)
            .expect_err("a page larger than the Router limit must fail closed");
        assert!(matches!(error, MigrationError::Remote(message) if message.contains("oversized")));
    }

    #[test]
    fn status_rejects_more_than_the_router_record_limit() {
        let local = test_artifacts(MAX_SCHEMA_MIGRATIONS);
        let mut pages = Vec::new();
        for page_index in 0..=MAX_SCHEMA_MIGRATIONS / MAX_SCHEMA_MIGRATION_LIST_LIMIT as usize {
            let count = MAX_SCHEMA_MIGRATION_LIST_LIMIT as usize;
            let start = page_index * count;
            let records = if start < local.len() {
                local[start..(start + count).min(local.len())]
                    .iter()
                    .map(test_record)
                    .collect()
            } else {
                vec![test_record(&local[0]); count]
            };
            let next = (page_index < MAX_SCHEMA_MIGRATIONS / count)
                .then(|| records.last().map(record_id).unwrap_or_default().to_owned());
            pages.push(test_page(records, next));
        }
        let mut transport = FakeMigrationTransport::new(pages);

        let plan = MigrationPlan { migrations: local };
        let error = status_for_plan(&plan, &mut transport)
            .expect_err("the Router ledger bound must be enforced by the CLI");
        assert!(matches!(error, MigrationError::Remote(message) if message.contains("more than")));
    }
}
