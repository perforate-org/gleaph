//! Gleaph prepared-query code generation.
//!
//! The generator consumes a versioned, graph-scoped manifest. The manifest is deliberately
//! independent from Router stable storage and planner internals; a future Router metadata
//! endpoint can use the same wire shape as local generation snapshots.

mod common;
mod ir;
mod javascript;
mod typescript;

pub use ir::{CodegenIr, OperationIr};
pub use javascript::generate_javascript;
pub use typescript::generate_typescript;

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The manifest format understood by this crate.
pub const MANIFEST_VERSION: u32 = 1;

/// A graph-scoped prepared-query metadata snapshot.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreparedManifest {
    pub manifest_version: u32,
    pub graph: GraphIdentity,
    pub operations: Vec<PreparedOperation>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GraphIdentity {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    Query,
    Update,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreparedOperation {
    pub name: String,
    pub kind: OperationKind,
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    pub result: ResultSchema,
    #[serde(default)]
    pub supports_consistency: bool,
    #[serde(default)]
    pub supports_idempotency: bool,
    #[serde(default)]
    pub allowed_sorts: Vec<SortKey>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Parameter {
    pub name: String,
    pub required: bool,
    #[serde(default)]
    pub nullable: bool,
    #[serde(rename = "type")]
    pub semantic_type: SemanticType,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResultSchema {
    pub columns: Vec<Column>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub semantic_type: SemanticType,
    #[serde(default)]
    pub nullable: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SortKey {
    pub key: String,
    #[serde(default)]
    pub label: Option<String>,
}

/// Language-neutral types supported by the initial manifest profiles.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SemanticType {
    Null,
    Bool,
    Int64,
    Uint64,
    Int128,
    Uint128,
    Int256,
    Uint256,
    Float64,
    Decimal,
    Text,
    Bytes,
    Date,
    Time,
    Principal,
    List { element: Box<SemanticType> },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest JSON is invalid: {0}")]
    Json(String),
    #[error("unsupported manifest version {0}; expected {MANIFEST_VERSION}")]
    UnsupportedVersion(u32),
    #[error("graph id must not be empty")]
    EmptyGraphId,
    #[error("manifest must contain at least one operation")]
    EmptyOperations,
    #[error("operation name must not be empty")]
    EmptyOperationName,
    #[error("duplicate operation name {0:?}")]
    DuplicateOperation(String),
    #[error("operation names {first:?} and {second:?} produce the same TypeScript identifier")]
    DuplicateGeneratedIdentifier { first: String, second: String },
    #[error("parameter name must not be empty in operation {0:?}")]
    EmptyParameterName(String),
    #[error("duplicate parameter {parameter:?} in operation {operation:?}")]
    DuplicateParameter {
        operation: String,
        parameter: String,
    },
    #[error("column name must not be empty in operation {0:?}")]
    EmptyColumnName(String),
    #[error("duplicate result column {column:?} in operation {operation:?}")]
    DuplicateColumn { operation: String, column: String },
    #[error("sort key must not be empty in operation {0:?}")]
    EmptySortKey(String),
    #[error("duplicate sort key {key:?} in operation {operation:?}")]
    DuplicateSortKey { operation: String, key: String },
    #[error("idempotency is only supported for update operation {0:?}")]
    IdempotencyOnQuery(String),
    #[error("selected runtime profile does not yet support {feature} for operation {operation:?}")]
    UnsupportedRuntimeFeature {
        operation: String,
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
    fn normalizes_operation_names_once_for_all_profiles() {
        let ir = manifest().normalize().unwrap();
        let operation = &ir.operations[0];
        assert_eq!(operation.wire_name, "find-users");
        assert_eq!(operation.type_name, "FindUsers");
        assert_eq!(operation.params_type_name, "FindUsersParams");
        assert_eq!(operation.row_type_name, "FindUsersRow");
    }
}
