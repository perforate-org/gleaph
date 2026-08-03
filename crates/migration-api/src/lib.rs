//! Versioned Router schema-migration wire contract.
//!
//! This crate owns the Candid-facing records shared by the Router and tooling.  It intentionally
//! contains no filesystem discovery, GQL parsing, stable-memory implementation, or transport
//! policy; those concerns remain with their owning crates.

use candid::{CandidType, Principal};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Current schema-migration wire version.
pub const SCHEMA_MIGRATION_API_VERSION: u32 = 1;
/// Fixed SHA-256 digest width.
pub const SCHEMA_MIGRATION_CHECKSUM_BYTES: usize = 32;
/// Maximum UTF-8 byte length of a migration id or parent id.
pub const MAX_SCHEMA_MIGRATION_ID_BYTES: usize = 128;
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

/// Compute the canonical execution checksum shared by the CLI and Router.
///
/// `statement` is the exact UTF-8 `up.gql` byte sequence, including comments and whitespace.
/// Description text, TOML formatting, and filesystem paths are deliberately outside this function
/// and therefore do not affect the digest.
pub fn schema_migration_checksum(
    id: &str,
    parent: Option<&str>,
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
}

/// Durable record retained by the Router migration ledger.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SchemaMigrationRecordV1 {
    /// Canonical six-digit migration identifier.
    pub id: String,
    /// Parent migration identifier, or `None` for the root.
    pub parent: Option<String>,
    /// Domain checksum of the immutable artifact.
    pub checksum: SchemaMigrationChecksum,
    /// Principal that authorized and applied the migration.
    pub actor: Principal,
    /// IC timestamp at which the Router recorded the migration.
    pub applied_at: u64,
    /// Exact UTF-8 GQL execution payload sent to the Router.
    pub statement: String,
    /// Narrow additive statement profile derived by the Router.
    pub profile: SchemaMigrationStatementProfile,
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
            b"CREATE GRAPH TYPE Social { NODE Person }\n",
        );
        assert_eq!(checksum.algorithm, SchemaMigrationChecksumAlgorithm::Sha256);
        assert_eq!(
            checksum.digest.as_slice(),
            &[
                0xaa, 0x64, 0x57, 0x73, 0x27, 0xeb, 0xae, 0x9e, 0x4a, 0x7f, 0x82, 0x68, 0x2d, 0x31,
                0x70, 0x07, 0xec, 0xa0, 0xe7, 0x48, 0xc7, 0x7f, 0x71, 0xc3, 0xda, 0xf4, 0x8a, 0x69,
                0xfb, 0xc9, 0x32, 0x61,
            ]
        );
    }

    #[test]
    fn checksum_fixed_vector_for_child_migration() {
        let checksum = schema_migration_checksum(
            "000002_bind",
            Some("000001_init"),
            b"CREATE GRAPH social TYPED Social\n",
        );
        assert_eq!(checksum.algorithm, SchemaMigrationChecksumAlgorithm::Sha256);
        assert_eq!(
            checksum.digest.as_slice(),
            &[
                0x11, 0xc8, 0x8e, 0xcd, 0x6b, 0x30, 0xe6, 0x8a, 0xa5, 0x6a, 0x04, 0xe4, 0x82, 0x14,
                0xaa, 0x57, 0xa2, 0x29, 0x59, 0x01, 0x45, 0xe7, 0x23, 0xd5, 0xfc, 0xe4, 0x53, 0xe4,
                0x7a, 0x4e, 0xe2, 0xb8,
            ]
        );
    }

    #[test]
    fn checksum_commits_every_typed_field_and_exact_statement_bytes() {
        let statement = b"// keep this comment\nCREATE GRAPH TYPE Social {}\n";
        let baseline = schema_migration_checksum("000002_social", Some("000001_init"), statement);

        assert_eq!(
            baseline,
            schema_migration_checksum("000002_social", Some("000001_init"), statement)
        );
        assert_ne!(
            baseline,
            schema_migration_checksum("000003_social", Some("000001_init"), statement)
        );
        assert_ne!(
            baseline,
            schema_migration_checksum("000002_social", None, statement)
        );
        assert_ne!(
            baseline,
            schema_migration_checksum("000002_social", Some("000001_other"), statement)
        );
        assert_ne!(
            baseline,
            schema_migration_checksum(
                "000002_social",
                Some("000001_init"),
                b"CREATE GRAPH TYPE Social {}\n"
            )
        );
        assert_ne!(
            baseline,
            schema_migration_checksum(
                "000002_social",
                Some("000001_init"),
                b"// keep this comment\nCREATE  GRAPH TYPE Social {}\n"
            )
        );
    }
}
