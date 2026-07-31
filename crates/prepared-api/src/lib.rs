//! Shared wire types for graph-scoped Gleaph prepared-query metadata.
//!
//! This crate owns the language-neutral manifest contract. Router implementations publish these
//! values, while code-generation profiles consume them without depending on Router internals.

#![warn(missing_docs)]

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Version of the prepared-query metadata contract.
pub const MANIFEST_VERSION: u32 = 1;

/// A graph-scoped prepared-query metadata snapshot.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreparedManifest {
    /// Version of the manifest wire shape.
    pub manifest_version: u32,
    /// Identity metadata for the logical graph described by this snapshot.
    pub graph: GraphIdentity,
    /// Prepared operations available in the snapshot.
    pub operations: Vec<PreparedOperation>,
}

/// Stable identity metadata for a logical graph.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub struct GraphIdentity {
    /// Stable graph identifier used by the Router.
    pub id: String,
    /// Optional human-readable graph name.
    #[serde(default)]
    pub name: Option<String>,
}

/// Execution mode of a prepared operation.
#[derive(Clone, Copy, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub enum OperationKind {
    /// Read-only prepared operation.
    Query,
    /// Mutating prepared operation.
    Update,
}

/// Metadata for one generated prepared operation.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreparedOperation {
    /// Operation name as exposed by the Router.
    pub name: String,
    /// Optional documentation emitted by code generators.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the operation is read-only or mutating.
    pub kind: OperationKind,
    /// Input parameters accepted by the operation.
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    /// Row schema returned by the operation.
    pub result: ResultSchema,
    /// Whether the operation accepts a consistency option.
    #[serde(default)]
    pub supports_consistency: bool,
    /// Whether the operation accepts an idempotency option.
    #[serde(default)]
    pub supports_idempotency: bool,
    /// Sort keys accepted by the operation.
    #[serde(default)]
    pub allowed_sorts: Vec<SortKey>,
}

/// Input parameter metadata for a prepared operation.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub struct Parameter {
    /// Parameter name used in the operation's wire representation.
    pub name: String,
    /// Optional documentation emitted by code generators.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether callers must provide this parameter.
    pub required: bool,
    /// Whether the parameter may explicitly contain null.
    #[serde(default)]
    pub nullable: bool,
    /// Language-neutral semantic type of the parameter.
    #[serde(rename = "type")]
    pub semantic_type: SemanticType,
}

/// Row schema returned by a prepared operation.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResultSchema {
    /// Columns in their returned order.
    pub columns: Vec<Column>,
}

/// One column in a prepared operation's result row.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub struct Column {
    /// Column name in the operation's wire representation.
    pub name: String,
    /// Language-neutral semantic type of the column.
    #[serde(rename = "type")]
    pub semantic_type: SemanticType,
    /// Whether the column may contain null.
    #[serde(default)]
    pub nullable: bool,
}

/// A caller-selectable sort key for a prepared operation.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub struct SortKey {
    /// Stable sort-key identifier sent to the Router.
    pub key: String,
    /// Optional human-readable label for the generated client.
    #[serde(default)]
    pub label: Option<String>,
}

/// A caller-selected ordering for a prepared query execution.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreparedSortSpec {
    /// Stable sort-key identifier declared in [`PreparedOperation::allowed_sorts`].
    pub key: String,
    /// Direction, accepted as `asc`, `ascending`, `desc`, or `descending`.
    pub direction: String,
}

/// Language-neutral type supported by the manifest profiles.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub enum SemanticType {
    /// The null type.
    Null,
    /// A boolean value.
    Bool,
    /// A signed 8-bit integer.
    Int8,
    /// A signed 16-bit integer.
    Int16,
    /// A signed 32-bit integer.
    Int32,
    /// A signed 64-bit integer.
    Int64,
    /// An unsigned 8-bit integer.
    Uint8,
    /// An unsigned 16-bit integer.
    Uint16,
    /// An unsigned 32-bit integer.
    Uint32,
    /// An unsigned 64-bit integer.
    Uint64,
    /// A signed 128-bit integer.
    Int128,
    /// An unsigned 128-bit integer.
    Uint128,
    /// A signed 256-bit integer.
    Int256,
    /// An unsigned 256-bit integer.
    Uint256,
    /// An IEEE 754 binary16 bit pattern.
    Float16,
    /// An IEEE 754 32-bit floating-point value.
    Float32,
    /// An IEEE 754 64-bit floating-point value.
    Float64,
    /// Canonical IEEE 754 binary128 bytes.
    Float128,
    /// Canonical IEEE 754 binary256 bytes.
    Float256,
    /// A decimal value represented by the runtime.
    Decimal,
    /// A UTF-8 text value.
    Text,
    /// An arbitrary byte sequence.
    Bytes,
    /// A calendar date.
    Date,
    /// A time value.
    Time,
    /// An Internet Computer principal.
    Principal,
    /// Nanoseconds since midnight in UTC.
    LocalTime,
    /// UTC date-time represented as seconds and nanoseconds.
    DateTime,
    /// Time-zone-free local date-time represented as seconds and nanoseconds.
    LocalDateTime,
    /// Date-time with an explicit UTC offset.
    ZonedDateTime,
    /// Time with an explicit UTC offset.
    ZonedTime,
    /// ISO-8601 duration represented as months and nanoseconds.
    Duration,
    /// A homogeneous list of values.
    List {
        /// Type of each list element.
        element: Box<SemanticType>,
    },
    /// A named-field record.
    Record {
        /// Fields in their wire order.
        fields: Vec<RecordField>,
    },
    /// A path consisting of vertex and edge identifiers.
    Path,
}

/// A field in a semantic record.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub struct RecordField {
    /// Field name in the wire representation.
    pub name: String,
    /// Field semantic type.
    #[serde(rename = "type")]
    pub semantic_type: SemanticType,
    /// Whether the field may contain null.
    #[serde(default)]
    pub nullable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_through_candid() {
        let manifest = PreparedManifest {
            manifest_version: MANIFEST_VERSION,
            graph: GraphIdentity {
                id: "default".into(),
                name: Some("Default".into()),
            },
            operations: vec![PreparedOperation {
                name: "find-users".into(),
                description: None,
                kind: OperationKind::Query,
                parameters: vec![Parameter {
                    name: "term".into(),
                    description: None,
                    required: true,
                    nullable: false,
                    semantic_type: SemanticType::Text,
                }],
                result: ResultSchema {
                    columns: vec![Column {
                        name: "user_name".into(),
                        semantic_type: SemanticType::Text,
                        nullable: false,
                    }],
                },
                supports_consistency: false,
                supports_idempotency: false,
                allowed_sorts: vec![],
            }],
        };

        let bytes = candid::encode_one(&manifest).expect("encode manifest");
        let decoded: PreparedManifest = candid::decode_one(&bytes).expect("decode manifest");
        assert_eq!(decoded, manifest);
    }
}
