//! Gleaph prepared-query code generation.
//!
//! The generator consumes a versioned, graph-scoped manifest. The manifest is deliberately
//! independent from Router stable storage and planner internals; a future Router metadata
//! endpoint can use the same wire shape as local generation snapshots.

#![warn(missing_docs)]

mod common;
mod ir;
mod javascript;
mod rust;
mod rust_canister;
mod typescript;

pub use ir::{CodegenIr, OperationIr};
pub use javascript::generate_javascript;
pub use rust::generate_rust;
pub use rust_canister::generate_rust_canister;
pub use typescript::generate_typescript;

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The manifest format understood by this crate.
pub const MANIFEST_VERSION: u32 = 2;

/// A graph-scoped prepared-query metadata snapshot.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreparedManifest {
    /// Version of the manifest wire shape.
    pub manifest_version: u32,
    /// Identity metadata for the logical graph described by this snapshot.
    pub graph: GraphIdentity,
    /// Prepared operations available in the snapshot.
    pub operations: Vec<PreparedOperation>,
}

/// Stable identity metadata for a logical graph.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GraphIdentity {
    /// Stable graph identifier used by the Router.
    pub id: String,
    /// Optional human-readable graph name.
    #[serde(default)]
    pub name: Option<String>,
}

/// Execution mode of a prepared operation.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    /// Read-only prepared operation.
    Query,
    /// Mutating prepared operation.
    Update,
}

/// Metadata for one generated prepared operation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreparedOperation {
    /// Operation name as exposed by the Router.
    pub name: String,
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Parameter {
    /// Parameter name used in the operation's wire representation.
    pub name: String,
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResultSchema {
    /// Columns in their returned order.
    pub columns: Vec<Column>,
}

/// One column in a prepared operation's result row.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SortKey {
    /// Stable sort-key identifier sent to the Router.
    pub key: String,
    /// Optional human-readable label for the generated client.
    #[serde(default)]
    pub label: Option<String>,
}

/// Language-neutral type supported by the manifest profiles.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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

/// Validation or profile error encountered while loading a manifest.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    /// The manifest was not valid JSON.
    #[error("manifest JSON is invalid: {0}")]
    Json(String),
    /// The manifest version is not supported by this crate.
    #[error("unsupported manifest version {0}; expected {MANIFEST_VERSION}")]
    UnsupportedVersion(u32),
    /// The graph identifier is empty or whitespace-only.
    #[error("graph id must not be empty")]
    EmptyGraphId,
    /// The manifest does not contain any operations.
    #[error("manifest must contain at least one operation")]
    EmptyOperations,
    /// An operation name is empty or whitespace-only.
    #[error("operation name must not be empty")]
    EmptyOperationName,
    /// Two operations use the same wire name.
    #[error("duplicate operation name {0:?}")]
    DuplicateOperation(String),
    /// Two operation names map to the same generated TypeScript identifier.
    #[error("operation names {first:?} and {second:?} produce the same TypeScript identifier")]
    DuplicateGeneratedIdentifier {
        /// First operation name that produced the generated identifier.
        first: String,
        /// Later operation name that produced the same generated identifier.
        second: String,
    },
    /// A parameter name is empty or whitespace-only.
    #[error("parameter name must not be empty in operation {0:?}")]
    EmptyParameterName(String),
    /// Two parameters in one operation use the same name.
    #[error("duplicate parameter {parameter:?} in operation {operation:?}")]
    DuplicateParameter {
        /// Operation containing the duplicate parameter.
        operation: String,
        /// Duplicate parameter name.
        parameter: String,
    },
    /// A result column name is empty or whitespace-only.
    #[error("column name must not be empty in operation {0:?}")]
    EmptyColumnName(String),
    /// Two result columns in one operation use the same name.
    #[error("duplicate result column {column:?} in operation {operation:?}")]
    DuplicateColumn {
        /// Operation containing the duplicate result column.
        operation: String,
        /// Duplicate result column name.
        column: String,
    },
    /// A sort key is empty or whitespace-only.
    #[error("sort key must not be empty in operation {0:?}")]
    EmptySortKey(String),
    /// Two sort keys in one operation use the same key.
    #[error("duplicate sort key {key:?} in operation {operation:?}")]
    DuplicateSortKey {
        /// Operation containing the duplicate sort key.
        operation: String,
        /// Duplicate sort-key identifier.
        key: String,
    },
    /// A record field name is empty or whitespace-only.
    #[error("record field name must not be empty in {0:?}")]
    EmptyRecordFieldName(String),
    /// Two fields in one record use the same name.
    #[error("duplicate record field {field:?} in {path:?}")]
    DuplicateRecordField {
        /// Path to the containing record.
        path: String,
        /// Duplicate field name.
        field: String,
    },
    /// Idempotency metadata was declared for a query operation.
    #[error("idempotency is only supported for update operation {0:?}")]
    IdempotencyOnQuery(String),
    /// The selected runtime profile does not implement a requested feature.
    #[error("selected runtime profile does not yet support {feature} for operation {operation:?}")]
    UnsupportedRuntimeFeature {
        /// Operation requesting the unsupported feature.
        operation: String,
        /// Name of the unsupported feature.
        feature: &'static str,
    },
}

impl PreparedManifest {
    /// Parse and validate a local manifest snapshot.
    pub fn from_json(input: &str) -> Result<Self, ManifestError> {
        let manifest: Self =
            serde_json::from_str(input).map_err(|e| ManifestError::Json(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate the manifest before it reaches a renderer.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.manifest_version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion(self.manifest_version));
        }
        if self.graph.id.trim().is_empty() {
            return Err(ManifestError::EmptyGraphId);
        }
        if self.operations.is_empty() {
            return Err(ManifestError::EmptyOperations);
        }

        let mut operation_names = BTreeSet::new();
        let mut generated_names: Vec<(String, String)> = Vec::new();
        for operation in &self.operations {
            if operation.name.trim().is_empty() {
                return Err(ManifestError::EmptyOperationName);
            }
            if !operation_names.insert(&operation.name) {
                return Err(ManifestError::DuplicateOperation(operation.name.clone()));
            }
            let generated_name = common::pascal_case(&operation.name);
            if let Some(first) = generated_names
                .iter()
                .find(|(name, _)| *name == generated_name)
            {
                return Err(ManifestError::DuplicateGeneratedIdentifier {
                    first: first.1.clone(),
                    second: operation.name.clone(),
                });
            }
            generated_names.push((generated_name, operation.name.clone()));
            if operation.supports_idempotency && operation.kind != OperationKind::Update {
                return Err(ManifestError::IdempotencyOnQuery(operation.name.clone()));
            }

            let mut parameter_names = BTreeSet::new();
            for parameter in &operation.parameters {
                if parameter.name.trim().is_empty() {
                    return Err(ManifestError::EmptyParameterName(operation.name.clone()));
                }
                if !parameter_names.insert(&parameter.name) {
                    return Err(ManifestError::DuplicateParameter {
                        operation: operation.name.clone(),
                        parameter: parameter.name.clone(),
                    });
                }
            }

            let mut column_names = BTreeSet::new();
            for column in &operation.result.columns {
                if column.name.trim().is_empty() {
                    return Err(ManifestError::EmptyColumnName(operation.name.clone()));
                }
                if !column_names.insert(&column.name) {
                    return Err(ManifestError::DuplicateColumn {
                        operation: operation.name.clone(),
                        column: column.name.clone(),
                    });
                }
            }

            for parameter in &operation.parameters {
                validate_semantic_type(
                    &parameter.semantic_type,
                    &format!(
                        "operation {:?} parameter {:?}",
                        operation.name, parameter.name
                    ),
                )?;
            }
            for column in &operation.result.columns {
                validate_semantic_type(
                    &column.semantic_type,
                    &format!("operation {:?} column {:?}", operation.name, column.name),
                )?;
            }

            let mut sort_keys = BTreeSet::new();
            for sort in &operation.allowed_sorts {
                if sort.key.trim().is_empty() {
                    return Err(ManifestError::EmptySortKey(operation.name.clone()));
                }
                if !sort_keys.insert(&sort.key) {
                    return Err(ManifestError::DuplicateSortKey {
                        operation: operation.name.clone(),
                        key: sort.key.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

fn validate_semantic_type(semantic_type: &SemanticType, path: &str) -> Result<(), ManifestError> {
    match semantic_type {
        SemanticType::List { element } => validate_semantic_type(element, path),
        SemanticType::Record { fields } => {
            let mut names = BTreeSet::new();
            for field in fields {
                if field.name.trim().is_empty() {
                    return Err(ManifestError::EmptyRecordFieldName(path.to_owned()));
                }
                if !names.insert(&field.name) {
                    return Err(ManifestError::DuplicateRecordField {
                        path: path.to_owned(),
                        field: field.name.clone(),
                    });
                }
                validate_semantic_type(
                    &field.semantic_type,
                    &format!("{path} record field {:?}", field.name),
                )?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PreparedManifest {
        PreparedManifest {
            manifest_version: MANIFEST_VERSION,
            graph: GraphIdentity {
                id: "default".into(),
                name: None,
            },
            operations: vec![PreparedOperation {
                name: "find-users".into(),
                kind: OperationKind::Query,
                parameters: vec![Parameter {
                    name: "term".into(),
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
        }
    }

    #[test]
    fn parses_and_validates_manifest() {
        let input = serde_json::to_string(&manifest()).unwrap();
        assert_eq!(PreparedManifest::from_json(&input).unwrap(), manifest());
    }

    #[test]
    fn exact_manifest_schema_uses_version_two() {
        assert_eq!(MANIFEST_VERSION, 2);

        let mut value = manifest();
        value.manifest_version = 1;
        assert_eq!(value.validate(), Err(ManifestError::UnsupportedVersion(1)));
    }

    #[test]
    fn rejects_duplicate_operation_names() {
        let mut value = manifest();
        value.operations.push(value.operations[0].clone());
        assert_eq!(
            value.validate(),
            Err(ManifestError::DuplicateOperation("find-users".into()))
        );
    }

    #[test]
    fn generates_typed_adapter_using_sdk_runtime() {
        let output = generate_typescript(&manifest()).unwrap();
        assert!(output.contains("export interface FindUsersParams"));
        assert!(output.contains("client.executePrepared(\"find-users\""));
        assert!(output.contains("fromApiValue(row[\"user_name\"])"));
        assert!(!output.contains("ic-agent"));
    }

    #[test]
    fn generates_runtime_javascript_without_type_syntax() {
        let output = generate_javascript(&manifest()).unwrap();
        assert!(output.contains("export function withPreparedQueries(client)"));
        assert!(output.contains("client.executePrepared(\"find-users\""));
        assert!(!output.contains("interface FindUsersParams"));
        assert!(!output.contains(" as const"));
    }

    #[test]
    fn generates_rust_facade_with_explicit_operation_kind() {
        let mut value = manifest();
        value.operations[0].parameters.clear();
        value.operations[0].allowed_sorts.push(SortKey {
            key: "user_name".into(),
            label: None,
        });
        let output = generate_rust(&value).unwrap();
        assert!(output.contains("pub struct FindUsersParams"));
        assert!(output.contains("pub struct FindUsersRow"));
        assert!(output.contains("self.executor.execute_query::<FindUsersRow>"));
        assert!(
            output.contains("pub async fn find_users(&self, sort: Option<Vec<PreparedSortSpec>>)")
        );
        assert!(!output.contains("&self, ,"));
        assert!(output.contains("pub trait PreparedExecutor"));
        assert!(!output.contains("ic-agent"));
    }

    #[test]
    fn generates_rust_canister_facade_with_runtime_boundary() {
        let mut value = manifest();
        value.operations[0].result.columns[0].semantic_type = SemanticType::Record {
            fields: vec![RecordField {
                name: "display".into(),
                semantic_type: SemanticType::Text,
                nullable: false,
            }],
        };
        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains("pub trait PreparedCanisterExecutor"));
        assert!(output.contains("fn encode_params<T: Serialize>"));
        assert!(output.contains("pub struct PreparedCanisterQueries"));
        assert!(output.contains("use gleaph_cdk::GqlParams"));
        assert!(output.contains("fn execute_gql<'a, Row>"));
        assert!(output.contains("execute_query::<FindUsersRow>"));
        assert!(!output.contains("use serde::"));
        assert!(output.contains("gleaph_cdk::serde_json::Value"));
        assert!(!output.contains("use serde_json"));
        assert!(!output.contains("ic-agent"));
    }

    #[test]
    fn generates_cdk_float_types_for_rust_canister_profile() {
        let mut value = manifest();
        value.operations[0].parameters = vec![
            Parameter {
                name: "quad".into(),
                required: true,
                nullable: false,
                semantic_type: SemanticType::Float128,
            },
            Parameter {
                name: "oct".into(),
                required: true,
                nullable: false,
                semantic_type: SemanticType::Float256,
            },
        ];
        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains("pub quad: gleaph_cdk::GqlFloat128"));
        assert!(output.contains("pub oct: gleaph_cdk::GqlFloat256"));
    }

    #[test]
    fn normalizes_operation_names_once_for_all_profiles() {
        let ir = manifest().normalize().unwrap();
        let operation = &ir.operations[0];
        assert_eq!(operation.wire_name, "find-users");
        assert_eq!(operation.type_name, "FindUsers");
        assert_eq!(operation.params_type_name, "FindUsersParams");
        assert_eq!(operation.row_type_name, "FindUsersRow");
    }

    #[test]
    fn preserves_exact_scalar_variants_in_all_profiles() {
        let mut value = manifest();
        value.operations[0].parameters = vec![
            Parameter {
                name: "small".into(),
                required: true,
                nullable: false,
                semantic_type: SemanticType::Int8,
            },
            Parameter {
                name: "wide_float".into(),
                required: true,
                nullable: false,
                semantic_type: SemanticType::Float128,
            },
        ];
        value.operations[0].result.columns[0].semantic_type = SemanticType::Uint32;

        let typescript = generate_typescript(&value).unwrap();
        assert!(typescript.contains("small: number"));
        assert!(typescript.contains("wide_float: Uint8Array"));
        assert!(typescript.contains("user_name: number"));

        let javascript = generate_javascript(&value).unwrap();
        assert!(javascript.contains("{ Int8: params[\"small\"] }"));
        assert!(javascript.contains("{ Float128: params[\"wide_float\"] }"));

        let rust = generate_rust(&value).unwrap();
        assert!(rust.contains("pub small: i8"));
        assert!(rust.contains("pub wide_float: Vec<u8>"));
        assert!(rust.contains("pub user_name: u32"));
    }

    #[test]
    fn supports_temporal_path_and_record_types_in_all_profiles() {
        let mut value = manifest();
        value.operations[0].parameters = vec![
            Parameter {
                name: "local_time".into(),
                required: true,
                nullable: false,
                semantic_type: SemanticType::LocalTime,
            },
            Parameter {
                name: "when".into(),
                required: true,
                nullable: false,
                semantic_type: SemanticType::ZonedDateTime,
            },
            Parameter {
                name: "metadata".into(),
                required: true,
                nullable: false,
                semantic_type: SemanticType::Record {
                    fields: vec![
                        RecordField {
                            name: "created_at".into(),
                            semantic_type: SemanticType::DateTime,
                            nullable: false,
                        },
                        RecordField {
                            name: "path".into(),
                            semantic_type: SemanticType::Path,
                            nullable: true,
                        },
                    ],
                },
            },
        ];
        value.operations[0].result.columns = vec![
            Column {
                name: "local".into(),
                semantic_type: SemanticType::LocalDateTime,
                nullable: false,
            },
            Column {
                name: "duration".into(),
                semantic_type: SemanticType::Duration,
                nullable: false,
            },
            Column {
                name: "zoned_time".into(),
                semantic_type: SemanticType::ZonedTime,
                nullable: false,
            },
        ];

        let typescript = generate_typescript(&value).unwrap();
        assert!(typescript.contains("local_time: bigint | number"));
        assert!(typescript.contains("when: { seconds: bigint | number"));
        assert!(typescript.contains("metadata: { created_at: { seconds: bigint | number"));
        assert!(typescript.contains("local: { seconds: bigint | number"));
        assert!(typescript.contains("duration: { months: number"));
        assert!(typescript.contains("zoned_time: { nanos: bigint | number"));

        let javascript = generate_javascript(&value).unwrap();
        assert!(javascript.contains(r#"{ LocalTime: params["local_time"] }"#));
        assert!(javascript.contains(r#"{ ZonedDateTime: params["when"] }"#));
        assert!(javascript.contains(r#"{ DateTime: params["metadata"]["created_at"] }"#));
        assert!(javascript.contains(r#"{ Path: params["metadata"]["path"] }"#));

        let rust = generate_rust(&value).unwrap();
        assert!(rust.contains("pub local_time: u64"));
        assert!(rust.contains("pub when: PreparedZonedDateTime"));
        assert!(rust.contains("pub metadata: BTreeMap<String, serde_json::Value>"));
        assert!(rust.contains("pub local: PreparedDateTime"));
        assert!(rust.contains("pub duration: PreparedDuration"));
        assert!(rust.contains("pub zoned_time: PreparedZonedTime"));
        assert!(!rust.contains("\n+"));
    }

    #[test]
    fn rejects_duplicate_nested_record_fields() {
        let mut value = manifest();
        value.operations[0].parameters[0].semantic_type = SemanticType::Record {
            fields: vec![
                RecordField {
                    name: "value".into(),
                    semantic_type: SemanticType::Int32,
                    nullable: false,
                },
                RecordField {
                    name: "value".into(),
                    semantic_type: SemanticType::Int32,
                    nullable: false,
                },
            ],
        };
        assert!(matches!(
            value.validate(),
            Err(ManifestError::DuplicateRecordField { field, .. }) if field == "value"
        ));
    }
}
