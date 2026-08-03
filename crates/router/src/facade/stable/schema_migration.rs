//! Durable Router schema-migration ledger (ADR 0058).
//!
//! The map in [`super::ROUTER_SCHEMA_MIGRATIONS`] is the sole canonical ledger.  Each value keeps
//! its parent link, while the linear head and root-to-head order are derived from those links on
//! every read/write.  This deliberately avoids a second head/index region that could diverge from
//! the immutable records.

use candid::{decode_one, encode_one};
use gleaph_migration_api::SchemaMigrationRecord;
pub(crate) use gleaph_migration_api::{
    MAX_SCHEMA_MIGRATION_GRAPH_NAME_BYTES, MAX_SCHEMA_MIGRATION_ID_BYTES,
    MAX_SCHEMA_MIGRATION_LIST_LIMIT, MAX_SCHEMA_MIGRATION_STATEMENT_BYTES, MAX_SCHEMA_MIGRATIONS,
    SCHEMA_MIGRATION_CHECKSUM_BYTES,
};
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;

/// Router-local stable representation of the shared wire record.
///
/// The wire crate intentionally does not depend on `ic-stable-structures`; this newtype keeps
/// persistence policy in the Router while still storing exactly the versioned public record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StableSchemaMigrationRecord(pub(crate) SchemaMigrationRecord);

/// Conservative allowance for the fixed Candid envelope: DIDL header/type table, version/option
/// variants, LEB128 lengths, checksum algorithm, principal, timestamp, and statement profile.
/// Variable payload bytes are accounted for separately from the shared public limits below.
const SCHEMA_MIGRATION_RECORD_CANDID_OVERHEAD_BYTES: u32 = 4 * 1024;

/// Maximum encoded stable value size. A record contains one id, an optional parent id, one fixed
/// checksum, one statement, and selector/resolved graph names; all other current fields fit inside
/// the conservative Candid envelope allowance.
pub(crate) const MAX_SCHEMA_MIGRATION_RECORD_BYTES: u32 = MAX_SCHEMA_MIGRATION_STATEMENT_BYTES
    as u32
    + 2 * MAX_SCHEMA_MIGRATION_ID_BYTES as u32
    + 2 * MAX_SCHEMA_MIGRATION_GRAPH_NAME_BYTES as u32
    + SCHEMA_MIGRATION_CHECKSUM_BYTES as u32
    + 16
    + SCHEMA_MIGRATION_RECORD_CANDID_OVERHEAD_BYTES;

impl Storable for StableSchemaMigrationRecord {
    const BOUND: Bound = Bound::Bounded {
        max_size: MAX_SCHEMA_MIGRATION_RECORD_BYTES,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(encode_one(&self.0).expect("encode SchemaMigrationRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        encode_one(&self.0).expect("encode SchemaMigrationRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(decode_one(bytes.as_ref()).expect("decode SchemaMigrationRecord"))
    }
}
