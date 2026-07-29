//! Gleaph prepared-query code generation.
//!
//! The generator consumes a versioned, graph-scoped manifest. The manifest is deliberately
//! independent from Router stable storage and planner internals; a future Router metadata
//! endpoint can use the same wire shape as local generation snapshots.

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

/// Language-neutral types supported by the initial manifest profile.
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
    #[error("TypeScript profile does not yet support {feature} for operation {operation:?}")]
    UnsupportedTypescriptFeature {
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
            let generated_name = pascal_case(&operation.name);
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

/// Generate a TypeScript prepared adapter over the existing `@gleaph/sdk` GraphClient.
///
/// The generated adapter owns only operation-specific types and decoding. Transport, Candid,
/// authorization, and common errors remain in the SDK as required by ADR 0053.
pub fn generate_typescript(manifest: &PreparedManifest) -> Result<String, ManifestError> {
    manifest.validate()?;
    for operation in &manifest.operations {
        if operation.supports_consistency {
            return Err(ManifestError::UnsupportedTypescriptFeature {
                operation: operation.name.clone(),
                feature: "read consistency options",
            });
        }
        if operation.supports_idempotency {
            return Err(ManifestError::UnsupportedTypescriptFeature {
                operation: operation.name.clone(),
                feature: "idempotent updates",
            });
        }
    }
    let mut out = String::from("// Generated by gleaph-codegen. Do not edit.\n\n");
    out.push_str(
        "import type { ApiQueryResponse, ApiValue, GraphClient, PreparedSortSpec } from \"@gleaph/sdk\";\n",
    );
    out.push_str("import { fromApiValue } from \"@gleaph/sdk\";\n\n");
    out.push_str("export const GLEAPH_GRAPH_ID = ");
    out.push_str(&json_string(&manifest.graph.id));
    out.push_str(" as const;\n\n");
    out.push_str("type PreparedResponse<Row> = Omit<ApiQueryResponse, \"execution\"> & { execution: Omit<ApiQueryResponse[\"execution\"], \"rows\"> & { rows: Row[] } };\n\n");
    out.push_str("export type PrincipalLike = string | { toText(): string };\n\n");

    for operation in &manifest.operations {
        let base = pascal_case(&operation.name);
        out.push_str(&format!("export interface {base}Params {{\n"));
        for parameter in &operation.parameters {
            let optional = if parameter.required { "" } else { "?" };
            let ty = ts_type(&parameter.semantic_type, parameter.nullable);
            out.push_str(&format!(
                "  {}{}: {};\n",
                ts_property(&parameter.name),
                optional,
                ty
            ));
        }
        out.push_str("}\n\n");
        out.push_str(&format!("export interface {base}Row {{\n"));
        for column in &operation.result.columns {
            out.push_str(&format!(
                "  {}: {};\n",
                ts_property(&column.name),
                ts_type(&column.semantic_type, column.nullable)
            ));
        }
        out.push_str("}\n\n");
    }

    out.push_str("export interface PreparedQueries {\n");
    for operation in &manifest.operations {
        let base = pascal_case(&operation.name);
        let params = format!("{base}Params");
        let row = format!("{base}Row");
        let optional = if operation.parameters.is_empty() {
            "?"
        } else {
            ""
        };
        let sort = if operation.allowed_sorts.is_empty() {
            ""
        } else {
            ", sort?: PreparedSortSpec[]"
        };
        out.push_str(&format!(
            "  {}: (params{}: {}{}) => Promise<PreparedResponse<{}>>;\n",
            ts_property(&operation.name),
            optional,
            params,
            sort,
            row
        ));
    }
    out.push_str("}\n\n");
    out.push_str(
        "export function withPreparedQueries(client: GraphClient): PreparedQueries {\n  return {\n",
    );
    for operation in &manifest.operations {
        let method = match operation.kind {
            OperationKind::Query => "executePrepared",
            OperationKind::Update => "executePreparedMutation",
        };
        let params = if operation.parameters.is_empty() {
            "params = {}"
        } else {
            "params"
        };
        let sort = if operation.allowed_sorts.is_empty() {
            ""
        } else {
            ", sort?: PreparedSortSpec[]"
        };
        out.push_str(&format!(
            "    {}: async ({params}{sort}) => {{\n",
            ts_property(&operation.name),
        ));
        out.push_str("      const encodedParams: Record<string, ApiValue> = {};\n");
        for parameter in &operation.parameters {
            let key = json_string(&parameter.name);
            let value = encode_expression(&format!("params[{key}]"), &parameter.semantic_type);
            if parameter.required {
                out.push_str(&format!("      encodedParams[{key}] = {value};\n"));
            } else {
                out.push_str(&format!(
                    "      if (params[{key}] !== undefined) encodedParams[{key}] = {value};\n"
                ));
            }
        }
        out.push_str(&format!(
            "      const response = await client.{method}({}, encodedParams{});\n",
            json_string(&operation.name),
            if operation.allowed_sorts.is_empty() {
                ""
            } else {
                ", sort"
            },
        ));
        out.push_str("      return { ...response, execution: { ...response.execution, rows: response.execution.rows.map((row) => ({\n");
        for column in &operation.result.columns {
            out.push_str(&format!(
                "        {}: fromApiValue(row[{}]) as {},\n",
                ts_property(&column.name),
                json_string(&column.name),
                ts_type(&column.semantic_type, column.nullable)
            ));
        }
        out.push_str("      })) } };\n    },\n");
    }
    out.push_str("  };\n}\n");
    Ok(out)
}

fn ts_type(semantic_type: &SemanticType, nullable: bool) -> String {
    let base = match semantic_type {
        SemanticType::Null => "null".to_string(),
        SemanticType::Bool => "boolean".to_string(),
        SemanticType::Int64
        | SemanticType::Uint64
        | SemanticType::Int128
        | SemanticType::Uint128 => "bigint | number".to_string(),
        SemanticType::Int256 | SemanticType::Uint256 | SemanticType::Decimal => {
            "string".to_string()
        }
        SemanticType::Float64 | SemanticType::Date => "number".to_string(),
        SemanticType::Time => "bigint | number".to_string(),
        SemanticType::Text => "string".to_string(),
        SemanticType::Principal => "PrincipalLike".to_string(),
        SemanticType::Bytes => "Uint8Array".to_string(),
        SemanticType::List { element } => format!("Array<{}>", ts_type(element, false)),
    };
    if nullable {
        format!("{base} | null")
    } else {
        base
    }
}

fn encode_expression(access: &str, semantic_type: &SemanticType) -> String {
    match semantic_type {
        SemanticType::Null => "{ Null: null }".to_string(),
        SemanticType::Bool => format!("{{ Bool: {access} }}"),
        SemanticType::Int64 => format!("{{ Int64: {access} }}"),
        SemanticType::Uint64 => format!("{{ Uint64: {access} }}"),
        SemanticType::Int128 => format!("{{ Int128: {access} }}"),
        SemanticType::Uint128 => format!("{{ Uint128: {access} }}"),
        SemanticType::Int256 => format!("{{ Int256: {access} }}"),
        SemanticType::Uint256 => format!("{{ Uint256: {access} }}"),
        SemanticType::Float64 => format!("{{ Float64: {access} }}"),
        SemanticType::Decimal => format!("{{ Decimal: {access} }}"),
        SemanticType::Text => format!("{{ Text: {access} }}"),
        SemanticType::Bytes => format!("{{ Bytes: {access} }}"),
        SemanticType::Date => format!("{{ Date: {access} }}"),
        SemanticType::Time => format!("{{ Time: {access} }}"),
        SemanticType::Principal => format!("{{ Principal: {access} }}"),
        SemanticType::List { element } => format!(
            "{{ List: {access}.map((value) => {}) }}",
            encode_expression("value", element)
        ),
    }
}

fn ts_property(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !name.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        name.to_string()
    } else {
        format!("{name:?}")
    }
}

fn pascal_case(name: &str) -> String {
    let mut result = String::new();
    for part in name.split(|c: char| !c.is_ascii_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            result.push(first.to_ascii_uppercase());
            result.extend(chars);
        }
    }
    if result.is_empty() {
        "PreparedOperation".to_string()
    } else if result.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("Operation{result}")
    } else {
        result
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
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
}
