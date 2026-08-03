//! Versioned Router schema-migration wire contract.
//!
//! This crate owns the Candid-facing records shared by the Router and tooling.  It intentionally
//! contains no filesystem discovery, GQL parsing, stable-memory implementation, or transport
//! policy; those concerns remain with their owning crates.

use candid::{CandidType, Principal};
pub use gleaph_graph_kernel::entry::{GraphId, IndexNameId};
pub use gleaph_graph_kernel::index::PhysicalIndexId;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Current schema-migration wire version.
pub const SCHEMA_MIGRATION_API_VERSION: u32 = 1;
/// Fixed SHA-256 digest width.
pub const SCHEMA_MIGRATION_CHECKSUM_BYTES: usize = 32;
/// Maximum UTF-8 byte length of a migration id or parent id.
pub const MAX_SCHEMA_MIGRATION_ID_BYTES: usize = 128;
/// Maximum UTF-8 byte length accepted for a named graph selector.
pub const MAX_SCHEMA_MIGRATION_GRAPH_NAME_BYTES: usize = 256;
/// Maximum UTF-8 byte length of one raw GQL statement.
pub const MAX_SCHEMA_MIGRATION_STATEMENT_BYTES: usize = 65_536;
/// Maximum number of records retained by the Router ledger.
pub const MAX_SCHEMA_MIGRATIONS: usize = 4_096;
/// Maximum page size accepted by the Router list method.
pub const MAX_SCHEMA_MIGRATION_LIST_LIMIT: u16 = 16;
/// Domain separator for the v1 execution checksum preimage.
pub const SCHEMA_MIGRATION_CHECKSUM_DOMAIN: &[u8] = b"gleaph-schema-migration\0v1\0";

/// Validate a v1 migration id and return its six-digit numeric sequence.
///
/// The accepted grammar is `[0-9]{6}_[a-z][a-z0-9]*(?:_[a-z0-9]+)*` with the
/// shared maximum id width.  Returning the sequence from the same validation
/// keeps Router and tooling from maintaining separate grammar implementations.
pub fn parse_schema_migration_id(id: &str) -> Option<u32> {
    if id.is_empty() || id.len() > MAX_SCHEMA_MIGRATION_ID_BYTES || !id.is_ascii() {
        return None;
    }
    let bytes = id.as_bytes();
    if bytes.len() < 8 || bytes[..6].iter().any(|byte| !byte.is_ascii_digit()) || bytes[6] != b'_' {
        return None;
    }
    let mut segments = id[7..].split('_');
    let first = segments.next()?;
    if first.is_empty()
        || !first.as_bytes()[0].is_ascii_lowercase()
        || !first
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || segments.any(|segment| {
            segment.is_empty()
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
    {
        return None;
    }
    Some(bytes[..6].iter().fold(0_u32, |sequence, byte| {
        sequence * 10 + u32::from(byte - b'0')
    }))
}

/// The checksum algorithm used by schema-migration artifacts.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum SchemaMigrationChecksumAlgorithm {
    /// Domain-separated SHA-256 over the v1 typed execution commitment.
    Sha256,
}

/// A versioned migration checksum.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SchemaMigrationChecksum {
    /// Algorithm/profile identifier.
    pub algorithm: SchemaMigrationChecksumAlgorithm,
    /// Raw digest bytes. The Router validates the exact width before persistence.
    pub digest: Vec<u8>,
}

/// Graph target selector carried by a migration artifact.
///
/// `Default` is an explicit wire and checksum value, not an absent field.  The Router resolves
/// the selector once before any migration-owned effect and persists the resulting graph identity
/// on records that enter a graph-specific lifecycle.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum SchemaMigrationGraphSelector {
    /// Resolve the Router's canonical default graph.
    Default,
    /// Resolve one graph by its canonical catalog name.
    Named(String),
}

/// Canonical graph identity resolved by Router from a migration selector.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ResolvedSchemaMigrationGraph {
    /// Router-issued logical graph identity.
    pub graph_id: GraphId,
    /// Canonical graph name captured with the id for audit and replay diagnostics.
    pub graph_name: String,
}

/// Compute the canonical execution checksum shared by the CLI and Router.
///
/// `statement` is the exact UTF-8 `up.gql` byte sequence, including comments and whitespace.
/// Description text, TOML formatting, and filesystem paths are deliberately outside this function
/// and therefore do not affect the digest.
pub fn schema_migration_checksum(
    id: &str,
    parent: Option<&str>,
    graph_selector: &SchemaMigrationGraphSelector,
    statement: &[u8],
) -> SchemaMigrationChecksum {
    let mut hasher = Sha256::new();
    hasher.update(SCHEMA_MIGRATION_CHECKSUM_DOMAIN);
    frame(&mut hasher, &1_u32.to_be_bytes());
    frame(&mut hasher, id.as_bytes());
    match parent {
        Some(parent) => {
            hasher.update([1]);
            frame(&mut hasher, parent.as_bytes());
        }
        None => hasher.update([0]),
    }
    match graph_selector {
        SchemaMigrationGraphSelector::Default => hasher.update([0]),
        SchemaMigrationGraphSelector::Named(name) => {
            hasher.update([1]);
            frame(&mut hasher, name.as_bytes());
        }
    }
    frame(&mut hasher, statement);
    SchemaMigrationChecksum {
        algorithm: SchemaMigrationChecksumAlgorithm::Sha256,
        digest: hasher.finalize().to_vec(),
    }
}

fn frame(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// Typed execution profile retained for audit and dispatch classification.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum SchemaMigrationStatementProfile {
    /// `CREATE GRAPH TYPE <name> { ... }`.
    CreateGraphType,
    /// `CREATE GRAPH <name> TYPED <type>`.
    CreateTypedGraph,
    /// Gleaph `CREATE INDEX` migration, which requires a separate Router backfill lifecycle.
    CreateIndex,
}

/// Compact terminal reason for a migration-driven index build.
///
/// Operational detail belongs in Router logs. Persisting only this closed enum keeps the durable
/// protocol bounded and prevents transport text from becoming part of migration identity.
#[derive(CandidType, Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum MigrationFailureCode {
    /// The live shard/index target set no longer matches the immutable preparation snapshot.
    TopologyChanged,
    /// A downstream response did not match the persisted graph, namespace, epoch, or target.
    StaleOrMismatchedResponse,
    /// A downstream owner deterministically rejected the immutable build contract.
    TargetRejected,
}

/// Durable terminal/pending state of one migration ledger record.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum SchemaMigrationRecordState {
    /// A migration-driven index build is pending. Detailed phase/progress remains owned by the
    /// Router physical-index lifecycle record addressed by this immutable pointer.
    PendingIndex {
        /// Graph-scoped logical index name id.
        index_name_id: IndexNameId,
        /// Never-reused physical posting namespace and build generation.
        physical_index_id: PhysicalIndexId,
    },
    /// The migration completed successfully.
    Applied {
        /// IC timestamp at which the terminal state was committed.
        applied_at: u64,
    },
    /// Deterministic failure whose cleanup completed and released the global pending gate.
    Failed {
        /// IC timestamp at which cleanup and the terminal state were committed.
        failed_at: u64,
        /// Bounded machine-readable failure classification.
        code: MigrationFailureCode,
    },
}

/// Durable record retained by the Router migration ledger.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SchemaMigrationRecordV1 {
    /// Canonical six-digit migration identifier.
    pub id: String,
    /// Parent migration identifier, or `None` for the root.
    pub parent: Option<String>,
    /// Graph selector committed by the migration artifact; omission is represented as `Default`.
    pub graph_selector: SchemaMigrationGraphSelector,
    /// Router-resolved graph identity when a graph-specific lifecycle has resolved the selector.
    pub resolved_graph: Option<ResolvedSchemaMigrationGraph>,
    /// Domain checksum of the immutable artifact.
    pub checksum: SchemaMigrationChecksum,
    /// Principal that authorized and applied the migration.
    pub actor: Principal,
    /// IC timestamp at which the Router first recorded the immutable migration envelope.
    pub recorded_at: u64,
    /// Exact UTF-8 GQL execution payload sent to the Router.
    pub statement: String,
    /// Narrow additive statement profile derived by the Router.
    pub profile: SchemaMigrationStatementProfile,
    /// Pending or terminal lifecycle state. CREATE INDEX phase detail is derived from the Router
    /// physical-index catalog rather than duplicated here.
    pub state: SchemaMigrationRecordState,
}

/// Current durable migration record shape.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum SchemaMigrationRecord {
    /// Version 1 record.
    V1(SchemaMigrationRecordV1),
}

/// Versioned apply request envelope.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum ApplySchemaMigrationArgs {
    /// Version 1 request.
    V1(ApplySchemaMigrationArgsV1),
}

/// Version 1 apply request.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ApplySchemaMigrationArgsV1 {
    /// Migration identifier.
    pub id: String,
    /// Parent migration identifier, or `None` for the root.
    pub parent: Option<String>,
    /// Graph selector committed by the migration artifact; omission is represented as `Default`.
    pub graph_selector: SchemaMigrationGraphSelector,
    /// Domain checksum supplied by the caller.
    pub checksum: SchemaMigrationChecksum,
    /// Exact UTF-8 GQL execution payload.
    pub statement: String,
}

/// Outcome of an apply request.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum SchemaMigrationApplyStatus {
    /// The migration was newly recorded and executed.
    Applied,
    /// The same id/checksum was already recorded; no duplicate execution occurred.
    Replay,
    /// A migration-driven index build made one bounded unit of progress and remains resumable.
    Progress(SchemaMigrationProgress),
    /// Deterministic terminal failure after resumable cleanup completed.
    Failed(MigrationFailureCode),
}

/// Public phase of a pending migration-driven index build.
#[derive(CandidType, Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum SchemaMigrationProgressPhase {
    Preparing,
    Building,
    Sealing,
    Aborting,
}

/// Compact progress returned by one bounded apply call.
#[derive(CandidType, Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub struct SchemaMigrationProgress {
    pub phase: SchemaMigrationProgressPhase,
    pub completed_targets: u32,
    pub total_targets: u32,
}

impl std::fmt::Display for SchemaMigrationApplyStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Applied => formatter.write_str("applied"),
            Self::Replay => formatter.write_str("replay"),
            Self::Progress(progress) => write!(
                formatter,
                "{:?} {}/{}",
                progress.phase, progress.completed_targets, progress.total_targets
            ),
            Self::Failed(code) => write!(formatter, "failed: {code:?}"),
        }
    }
}

/// Versioned apply result envelope.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum ApplySchemaMigrationResult {
    /// Version 1 result.
    V1(ApplySchemaMigrationResultV1),
}

/// Version 1 apply result.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ApplySchemaMigrationResultV1 {
    /// Whether execution was new or an exact replay.
    pub status: SchemaMigrationApplyStatus,
    /// Canonical record retained after the operation.
    pub record: SchemaMigrationRecord,
}

/// Versioned list request envelope.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum ListSchemaMigrationsArgs {
    /// Version 1 request.
    V1(ListSchemaMigrationsArgsV1),
}

/// Version 1 list request.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ListSchemaMigrationsArgsV1 {
    /// Exclusive cursor identifying the last record returned by the prior page.
    pub start_after: Option<String>,
    /// Maximum records requested by the caller.
    pub limit: u16,
}

/// Versioned list result envelope.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum ListSchemaMigrationsResult {
    /// Version 1 result.
    V1(ListSchemaMigrationsResultV1),
}

/// Version 1 list result.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ListSchemaMigrationsResultV1 {
    /// Records in canonical parent order.
    pub migrations: Vec<SchemaMigrationRecord>,
    /// Cursor for the next page, if any.
    pub next_start_after: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_id_parser_validates_grammar_and_extracts_sequence() {
        assert_eq!(parse_schema_migration_id("000001_init"), Some(1));
        assert_eq!(
            parse_schema_migration_id("000042_add_social_graph"),
            Some(42)
        );
        assert_eq!(parse_schema_migration_id("999999_z9"), Some(999_999));

        for invalid in [
            "",
            "000001",
            "000001_",
            "000001_Init",
            "000001_init-graph",
            "000001_init__graph",
            "00001_init",
            "000001_init!",
            "000001_é",
        ] {
            assert_eq!(parse_schema_migration_id(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn migration_id_parser_enforces_shared_width_limit() {
        let valid = format!("000001_{}", "a".repeat(MAX_SCHEMA_MIGRATION_ID_BYTES - 7));
        assert_eq!(valid.len(), MAX_SCHEMA_MIGRATION_ID_BYTES);
        assert_eq!(parse_schema_migration_id(&valid), Some(1));

        let oversized = format!("000001_{}", "a".repeat(MAX_SCHEMA_MIGRATION_ID_BYTES - 6));
        assert_eq!(parse_schema_migration_id(&oversized), None);
    }

    #[test]
    fn checksum_fixed_vector_for_root_migration() {
        let checksum = schema_migration_checksum(
            "000001_init",
            None,
            &SchemaMigrationGraphSelector::Default,
            b"CREATE GRAPH TYPE Social { NODE Person }\n",
        );
        assert_eq!(checksum.algorithm, SchemaMigrationChecksumAlgorithm::Sha256);
        assert_eq!(
            checksum.digest.as_slice(),
            &[
                0xbd, 0x83, 0x83, 0xba, 0xd6, 0x9f, 0xcb, 0x13, 0xff, 0x7e, 0x2d, 0x8e, 0x69, 0x3c,
                0xd0, 0x90, 0xef, 0x39, 0xaf, 0xff, 0x35, 0x74, 0x8d, 0xdb, 0x00, 0x08, 0x35, 0xaa,
                0x59, 0x0a, 0x4a, 0x70,
            ]
        );
    }

    #[test]
    fn checksum_fixed_vector_for_child_migration() {
        let checksum = schema_migration_checksum(
            "000002_bind",
            Some("000001_init"),
            &SchemaMigrationGraphSelector::Default,
            b"CREATE GRAPH social TYPED Social\n",
        );
        assert_eq!(checksum.algorithm, SchemaMigrationChecksumAlgorithm::Sha256);
        assert_eq!(
            checksum.digest.as_slice(),
            &[
                0x95, 0x49, 0xe8, 0x6a, 0xeb, 0x34, 0x20, 0x5f, 0xc5, 0xe8, 0x81, 0x6f, 0xcd, 0x17,
                0x11, 0x02, 0xc4, 0xff, 0x36, 0x50, 0x3e, 0xdd, 0xb1, 0x6b, 0x49, 0x26, 0xdb, 0x49,
                0x76, 0xc9, 0xbf, 0x37,
            ]
        );
    }

    #[test]
    fn checksum_commits_every_typed_field_and_exact_statement_bytes() {
        let statement = b"// keep this comment\nCREATE GRAPH TYPE Social {}\n";
        let baseline = schema_migration_checksum(
            "000002_social",
            Some("000001_init"),
            &SchemaMigrationGraphSelector::Default,
            statement,
        );

        assert_eq!(
            baseline,
            schema_migration_checksum(
                "000002_social",
                Some("000001_init"),
                &SchemaMigrationGraphSelector::Default,
                statement,
            )
        );
        assert_ne!(
            baseline,
            schema_migration_checksum(
                "000003_social",
                Some("000001_init"),
                &SchemaMigrationGraphSelector::Default,
                statement,
            )
        );
        assert_ne!(
            baseline,
            schema_migration_checksum(
                "000002_social",
                None,
                &SchemaMigrationGraphSelector::Default,
                statement,
            )
        );
        assert_ne!(
            baseline,
            schema_migration_checksum(
                "000002_social",
                Some("000001_other"),
                &SchemaMigrationGraphSelector::Default,
                statement,
            )
        );
        assert_ne!(
            baseline,
            schema_migration_checksum(
                "000002_social",
                Some("000001_init"),
                &SchemaMigrationGraphSelector::Default,
                b"CREATE GRAPH TYPE Social {}\n"
            )
        );
        assert_ne!(
            baseline,
            schema_migration_checksum(
                "000002_social",
                Some("000001_init"),
                &SchemaMigrationGraphSelector::Default,
                b"// keep this comment\nCREATE  GRAPH TYPE Social {}\n"
            )
        );
    }

    #[test]
    fn checksum_commits_default_and_named_graph_selectors_distinctly() {
        let statement = b"CREATE INDEX person_age FOR (n:Person) ON (n.age)\n";
        let default = schema_migration_checksum(
            "000002_age_index",
            Some("000001_init"),
            &SchemaMigrationGraphSelector::Default,
            statement,
        );
        let named = schema_migration_checksum(
            "000002_age_index",
            Some("000001_init"),
            &SchemaMigrationGraphSelector::Named("social".into()),
            statement,
        );
        let other_named = schema_migration_checksum(
            "000002_age_index",
            Some("000001_init"),
            &SchemaMigrationGraphSelector::Named("other".into()),
            statement,
        );
        assert_ne!(default, named);
        assert_ne!(named, other_named);
        assert_eq!(
            named.digest.as_slice(),
            &[
                0x42, 0x6b, 0x0d, 0x1a, 0xdd, 0x7a, 0xd6, 0x50, 0x85, 0xb8, 0x8a, 0x13, 0xe9, 0x41,
                0x0d, 0xe8, 0x0f, 0xab, 0x25, 0x7c, 0xcf, 0xbc, 0xc5, 0x79, 0x64, 0x17, 0x92, 0x21,
                0x74, 0xd4, 0x49, 0x91,
            ]
        );
    }
}
