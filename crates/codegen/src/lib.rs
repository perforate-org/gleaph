//! Gleaph prepared-query code generation.
//!
//! The generator consumes a versioned, graph-scoped manifest. The manifest is deliberately
//! independent from Router stable storage and planner internals; a future Router metadata
//! endpoint can use the same wire shape as local generation snapshots.

#![warn(missing_docs)]

mod cli;
mod common;
mod ir;
mod javascript;
mod motoko;
mod rust;
mod typescript;

pub use ir::{CodegenIr, OperationIr};
pub use javascript::generate_javascript;
pub use motoko::generate_motoko;
pub use rust::canister::generate_rust_canister;
pub use rust::client::generate_rust;
pub use rust::format::{RustFormatMode, format_rust};
pub use typescript::generate_typescript;

pub use cli::{CodegenArgs, CodegenError, run};

use std::collections::BTreeSet;

pub use gleaph_prepared_api::{
    Column, GraphIdentity, MANIFEST_VERSION, OperationKind, Parameter, PreparedManifest,
    PreparedOperation, RecordField, ResultSchema, SemanticType, SortKey,
};

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

/// Parse and validate a local manifest snapshot.
pub fn parse_manifest(input: &str) -> Result<PreparedManifest, ManifestError> {
    let manifest: PreparedManifest =
        serde_json::from_str(input).map_err(|e| ManifestError::Json(e.to_string()))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Validate the manifest before it reaches a renderer.
pub fn validate_manifest(manifest: &PreparedManifest) -> Result<(), ManifestError> {
    if manifest.manifest_version != MANIFEST_VERSION {
        return Err(ManifestError::UnsupportedVersion(manifest.manifest_version));
    }
    if manifest.graph.id.trim().is_empty() {
        return Err(ManifestError::EmptyGraphId);
    }
    if manifest.operations.is_empty() {
        return Err(ManifestError::EmptyOperations);
    }

    let mut operation_names = BTreeSet::new();
    let mut generated_names: Vec<(String, String)> = Vec::new();
    for operation in &manifest.operations {
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
                description: Some("Find users by their search term.".into()),
                kind: OperationKind::Query,
                parameters: vec![Parameter {
                    name: "term".into(),
                    description: Some("Text to search for.".into()),
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

    fn compact(source: &str) -> String {
        source.split_whitespace().collect()
    }

    fn contains_compact(source: &str, expected: &str) -> bool {
        compact(source).contains(&compact(expected))
    }

    #[test]
    fn parses_and_validates_manifest() {
        let input = serde_json::to_string(&manifest()).unwrap();
        assert_eq!(parse_manifest(&input).unwrap(), manifest());
    }

    #[test]
    fn exact_manifest_schema_uses_version_one() {
        assert_eq!(MANIFEST_VERSION, 1);

        let mut value = manifest();
        value.manifest_version = 2;
        assert_eq!(
            validate_manifest(&value),
            Err(ManifestError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn rejects_duplicate_operation_names() {
        let mut value = manifest();
        value.operations.push(value.operations[0].clone());
        assert_eq!(
            validate_manifest(&value),
            Err(ManifestError::DuplicateOperation("find-users".into()))
        );
    }

    #[test]
    fn generates_typed_adapter_using_sdk_runtime() {
        let output = generate_typescript(&manifest()).unwrap();
        assert!(output.contains("export interface FindUsersParams"));
        assert!(output.contains("client.executePrepared(\"find-users\""));
        assert!(output.contains("fromApiValue(row[\"user_name\"])"));
        assert!(!output.contains("PreparedSortSpec"));
        assert!(!output.contains("ApiPathElement"));
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
    fn typescript_imports_follow_manifest_usage() {
        let mut value = manifest();
        let plain = generate_typescript(&value).unwrap();
        assert!(!plain.contains("PreparedSortSpec"));
        assert!(!plain.contains("ApiPathElement"));

        value.operations[0].allowed_sorts.push(SortKey {
            key: "user_name".into(),
            label: None,
        });
        value.operations[0].parameters[0].semantic_type = SemanticType::List {
            element: Box::new(SemanticType::Path),
        };
        let used = generate_typescript(&value).unwrap();
        assert!(used.contains("PreparedSortSpec"));
        assert!(used.contains("ApiPathElement"));
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
        assert!(output.contains(".execute_query::<FindUsersRow>"));
        assert!(output.contains(
            "pub async fn find_users(\n        &self,\n        sort: Option<Vec<PreparedSortSpec>>,"
        ));
        assert!(!output.contains("&self, ,"));
        assert!(output.contains("pub trait PreparedExecutor"));
        assert!(!output.contains("ic-agent"));
    }

    #[test]
    fn generates_rust_canister_profile_with_prepared_operations() {
        let mut value = manifest();
        value.operations[0].result.columns[0].semantic_type = SemanticType::Record {
            fields: vec![RecordField {
                name: "display".into(),
                semantic_type: SemanticType::Text,
                nullable: false,
            }],
        };
        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains("pub trait PreparedExt"));
        assert!(output.contains("pub struct Prepared;"));
        assert!(output.contains("impl PreparedExt for gleaph_cdk::GleaphClient<Prepared>"));
        assert!(output.contains("use gleaph_cdk::GqlParams"));
        assert!(output.contains("pub fn into_gql_params(self) -> GqlParams"));
        assert!(output.contains("fn find_users("));
        assert!(output.contains(
            "impl Future<Output = Result<PreparedResponse<FindUsersRow>, gleaph_cdk::CallError>> + Send;"
        ));
        assert!(output.contains(".prepared_query(\"find-users\""));
        assert!(!output.contains("use serde::"));
        assert!(output.contains("gleaph_cdk::serde_json::Value"));
        assert!(!output.contains("use serde_json"));
        assert!(!output.contains("ic-agent"));
    }

    #[test]
    fn generates_update_operations_wrap_prepared_mutate() {
        let mut value = manifest();
        value.operations[0].kind = OperationKind::Update;
        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains("client_mutation_key: impl Into<String>"));
        assert!(output.contains(".prepared_mutate(\"find-users\""));
        assert!(
            output.contains(
                "params: FindUsersParams,\n        client_mutation_key: impl Into<String>,"
            )
        );
    }

    #[test]
    fn generates_motoko_canister_facade_with_typed_operation_boundary() {
        let output = generate_motoko(&manifest()).unwrap();
        assert!(output.contains("public type FindUsersParams ="));
        assert!(output.contains("term : Text;"));
        assert!(output.contains("public type FindUsersExecutor ="));
        assert!(output.contains("executeQuery : (Text, FindUsersParams)"));
        assert!(output.contains("public class FindUsersQueries"));
        assert!(output.contains("/// Find users by their search term."));
        assert!(output.contains("/// Text to search for."));
        assert!(!output.contains("import Principal \"mo:core/Principal\";"));

        let mut principal_manifest = manifest();
        principal_manifest.operations[0].parameters[0].semantic_type = SemanticType::Principal;
        let principal_output = generate_motoko(&principal_manifest).unwrap();
        assert!(principal_output.contains("import Principal \"mo:core/Principal\";"));
    }

    #[test]
    fn generates_serde_row_decoding_in_canister_profile() {
        let output = generate_rust_canister(&manifest()).unwrap();
        assert!(output.contains("decode_serde_rows::<FindUsersRow>"));
        assert!(output.contains(
            "rows: result\n                .decode_serde_rows::<FindUsersRow>()?\n                .unwrap_or_default(),"
        ));
    }

    #[test]
    fn generates_record_columns_as_open_json_maps() {
        let mut value = manifest();
        value.operations[0].result.columns[0].semantic_type = SemanticType::Record {
            fields: vec![RecordField {
                name: "display".into(),
                semantic_type: SemanticType::Text,
                nullable: false,
            }],
        };

        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains(
            "pub user_name: std::collections::BTreeMap<String, gleaph_cdk::serde_json::Value>"
        ));
        assert!(output.contains("gleaph_cdk::serde_json::Value"));
    }

    #[test]
    fn generates_cdk_float_types_for_rust_canister_profile() {
        let mut value = manifest();
        value.operations[0].parameters = vec![
            Parameter {
                name: "quad".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Float128,
            },
            Parameter {
                name: "oct".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Float256,
            },
            Parameter {
                name: "signed".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Int256,
            },
            Parameter {
                name: "unsigned".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Uint256,
            },
            Parameter {
                name: "price".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Decimal,
            },
        ];
        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains("pub quad: gleaph_cdk::GqlFloat128"));
        assert!(output.contains("pub oct: gleaph_cdk::GqlFloat256"));
        assert!(output.contains("pub signed: gleaph_cdk::GqlInt256"));
        assert!(output.contains("pub unsigned: gleaph_cdk::GqlUint256"));
        assert!(output.contains("pub price: gleaph_cdk::GqlDecimal"));
        assert!(output.contains("#[serde(crate = \"gleaph_cdk::serde\")]"));
        assert!(output.contains("GqlValue::Float128(self.quad.into_inner())"));
        assert!(output.contains("GqlValue::Float256(self.oct.into_inner())"));
        assert!(output.contains("GqlValue::Int256(self.signed.into_inner())"));
        assert!(output.contains("GqlValue::Uint256(self.unsigned.into_inner())"));
        assert!(output.contains("GqlValue::Decimal(self.price.into_inner())"));
    }

    #[test]
    fn generates_record_params_as_shared_gql_records() {
        let mut value = manifest();
        value.operations[0].parameters[0].semantic_type = SemanticType::Record {
            fields: vec![RecordField {
                name: "term".into(),
                semantic_type: SemanticType::Text,
                nullable: false,
            }],
        };
        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains("pub term: gleaph_cdk::GqlRecord"));
        assert!(output.contains("GqlValue::Record(self.term)"));
        assert!(contains_compact(
            &output,
            "self.prepared_query(\"find-users\"",
        ));
    }

    #[test]
    fn generates_principal_and_path_gql_param_conversions() {
        let mut value = manifest();
        value.operations[0].parameters = vec![
            Parameter {
                name: "owner".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Principal,
            },
            Parameter {
                name: "route".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Path,
            },
        ];
        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains("pub owner: gleaph_cdk::GqlPrincipal"));
        assert!(output.contains("gleaph_cdk::gql_principal_value(self.owner)"));
        assert!(output.contains("GqlValue::Path(self.route.into_iter()"));
        assert!(output.contains("gleaph_cdk::GqlPathElement::Vertex"));
    }

    #[test]
    fn generates_temporal_and_list_gql_param_conversions() {
        let mut value = manifest();
        value.operations[0].parameters = vec![
            Parameter {
                name: "created_at".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::DateTime,
            },
            Parameter {
                name: "local_created_at".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::LocalDateTime,
            },
            Parameter {
                name: "window".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::ZonedDateTime,
            },
            Parameter {
                name: "duration".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Duration,
            },
            Parameter {
                name: "ids".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::List {
                    element: Box::new(SemanticType::Int32),
                },
            },
        ];
        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains("GqlValue::DateTime(self.created_at.seconds"));
        assert!(output.contains("GqlValue::LocalDateTime(self.local_created_at.seconds"));
        assert!(output.contains("GqlValue::ZonedDateTime(self.window.seconds"));
        assert!(output.contains("GqlValue::Duration(self.duration.months"));
        assert!(contains_compact(
            &output,
            "GqlValue::List(self.ids.into_iter().map(|value| GqlValue::Int32(value))",
        ));
    }

    #[test]
    fn generates_temporal_and_constructed_row_fields() {
        let mut value = manifest();
        value.operations[0].result.columns = vec![
            Column {
                name: "created_at".into(),
                semantic_type: SemanticType::DateTime,
                nullable: false,
            },
            Column {
                name: "owner".into(),
                semantic_type: SemanticType::Principal,
                nullable: false,
            },
            Column {
                name: "route".into(),
                semantic_type: SemanticType::Path,
                nullable: false,
            },
            Column {
                name: "ids".into(),
                semantic_type: SemanticType::List {
                    element: Box::new(SemanticType::Int32),
                },
                nullable: false,
            },
            Column {
                name: "deleted_at".into(),
                semantic_type: SemanticType::Null,
                nullable: false,
            },
        ];
        let output = generate_rust_canister(&value).unwrap();
        assert!(output.contains("pub created_at: PreparedDateTime"));
        assert!(output.contains("pub owner: gleaph_cdk::GqlPrincipal"));
        assert!(output.contains("pub route: Vec<PreparedPathElement>"));
        assert!(output.contains("pub ids: Vec<i32>"));
        assert!(output.contains("pub deleted_at: ()"));
    }

    #[test]
    fn normalizes_operation_names_once_for_all_profiles() {
        let ir = crate::ir::normalize_manifest(&manifest()).unwrap();
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
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Int8,
            },
            Parameter {
                name: "wide_float".into(),
                description: None,
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
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::LocalTime,
            },
            Parameter {
                name: "when".into(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::ZonedDateTime,
            },
            Parameter {
                name: "metadata".into(),
                description: None,
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
    fn emits_operation_and_parameter_documentation_in_all_profiles() {
        let value = manifest();
        let typescript = generate_typescript(&value).unwrap();
        assert!(typescript.contains("/**\n * Find users by their search term.\n */"));
        assert!(typescript.contains("/**\n   * Text to search for.\n   */"));

        let javascript = generate_javascript(&value).unwrap();
        assert!(javascript.contains("/**\n     * Find users by their search term.\n     */"));

        let rust = generate_rust(&value).unwrap();
        assert!(rust.contains("/// Find users by their search term."));
        assert!(rust.contains("/// Text to search for."));

        let rust_canister = generate_rust_canister(&value).unwrap();
        assert!(rust_canister.contains("/// Find users by their search term."));
        assert!(rust_canister.contains("/// Text to search for."));
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
            validate_manifest(&value),
            Err(ManifestError::DuplicateRecordField { field, .. }) if field == "value"
        ));
    }
}
