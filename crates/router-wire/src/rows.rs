//! Row-schema validation: decoded wire rows checked against a prepared operation's
//! declared result columns.
//!
//! The Rust mirror of the JS SDK's `decodeRow`/`decodeRows` schema checks. A generated
//! Rust client calls [`validate_result_rows`] before serde projection so a row whose
//! runtime shape drifted from the declared metadata fails with a precise schema error
//! (`declared Bytes but received List`) instead of a generic serde message. Both client
//! SDKs re-export this module, so one implementation serves the application client and
//! the canister CDK.

use gleaph_gql_ic_wire::{GqlWireRow, GqlWireValue};
use gleaph_prepared_api::{Column, SemanticType};
use crate::types::GqlQueryResult;

pub use gleaph_prepared_api::RecordField;

/// One row failed its declared column schema.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RowSchemaError {
    /// The wire row lacks a column the prepared metadata declares.
    #[error("row is missing column \"{column}\"")]
    MissingColumn {
        /// The declared column name absent from the row.
        column: String,
    },
    /// A null runtime value in a column declared non-null.
    #[error("column \"{column}\" is null but declared non-null")]
    NullColumn {
        /// The column that received null.
        column: String,
    },
    /// The wire value's runtime tag does not match the declared semantic type.
    #[error("column \"{column}\": declared {declared} but received {received}")]
    TypeMismatch {
        /// The column (or its path) where the mismatch surfaced.
        column: String,
        /// The declared semantic type tag.
        declared: String,
        /// The runtime wire value tag.
        received: String,
    },
    /// A declared record field is absent from the wire record.
    #[error("column \"{column}\": row is missing field \"{field}\"")]
    MissingField {
        /// The record-typed column.
        column: String,
        /// The declared field absent from the wire record.
        field: String,
    },
    /// A record field's runtime tag does not match its declared semantic type.
    #[error("column \"{column}\": field \"{field}\": declared {declared} but received {received}")]
    FieldTypeMismatch {
        /// The record-typed column.
        column: String,
        /// The field where the mismatch surfaced.
        field: String,
        /// The declared semantic type tag.
        declared: String,
        /// The runtime wire value tag.
        received: String,
    },
    /// A record field carries null but is declared non-null.
    #[error("column \"{column}\": field \"{field}\" is null but declared non-null")]
    NullField {
        /// The record-typed column.
        column: String,
        /// The field that received null.
        field: String,
    },
    /// A Candid-level rows decode failure, surfaced here so callers need one error channel.
    #[error("rows decode failed: {0}")]
    Decode(String),
}

/// Validate every row of a query result against the operation's declared columns.
///
/// The declared columns are the `result.columns` of a prepared-operation manifest. Rows
/// are Candid-decoded from the result's rows blob first; a decode failure maps to
/// [`RowSchemaError::Decode`]. A result without a rows blob (no rows materialized) passes.
pub fn validate_result_rows(
    result: &GqlQueryResult,
    columns: &[Column],
) -> Result<(), RowSchemaError> {
    let Some(rows) = result
        .decode_rows()
        .map_err(|error| RowSchemaError::Decode(error.to_string()))?
    else {
        return Ok(());
    };
    validate_rows(&rows.rows, columns)
}

/// Validate every row against the declared columns. Row order is preserved; validation
/// stops at the first failure.
pub fn validate_rows(rows: &[GqlWireRow], columns: &[Column]) -> Result<(), RowSchemaError> {
    for row in rows {
        validate_row(row, columns)?;
    }
    Ok(())
}

/// Validate one wire row against the declared columns.
///
/// Mirrors the JS SDK's `decodeRow` schema contract: every declared column must be
/// present in the row, the runtime value tag must match the declared semantic type
/// (`List`/`Record` recurse against the element/field schema), `Null` only appears in
/// nullable columns, and record fields carry the same checks. Columns present in the row
/// but absent from the schema are ignored.
pub fn validate_row(row: &GqlWireRow, columns: &[Column]) -> Result<(), RowSchemaError> {
    for column in columns {
        let Some((_, value)) = row
            .columns
            .iter()
            .find(|(name, _)| name == &column.name)
        else {
            return Err(RowSchemaError::MissingColumn {
                column: column.name.clone(),
            });
        };
        if matches!(value, GqlWireValue::Null) {
            if !column.nullable {
                return Err(RowSchemaError::NullColumn {
                    column: column.name.clone(),
                });
            }
            continue;
        }
        validate_value(value, &column.semantic_type, &column.name, None)?;
    }
    Ok(())
}

/// Validate one wire value against one semantic type.
///
/// `field` is `None` at the column level and `Some(name)` inside a record, so nested
/// errors keep their column context. `Null` passes at this layer — nullability is
/// enforced by the schema owner (the column and record-field checks above), exactly like
/// the JS decoder, which returns `null` before the type check.
fn validate_value(
    value: &GqlWireValue,
    ty: &SemanticType,
    column: &str,
    field: Option<&str>,
) -> Result<(), RowSchemaError> {
    match ty {
        SemanticType::List { element } => match value {
            GqlWireValue::List(items) => {
                for item in items {
                    validate_value(item, element, column, field)?;
                }
                Ok(())
            }
            other => Err(type_mismatch(other, ty, column, field)),
        },
        SemanticType::Record { fields } => match value {
            GqlWireValue::Record(wire_fields) => {
                for field in fields {
                    let Some((_, wire)) = wire_fields
                        .iter()
                        .find(|(name, _)| name == &field.name)
                    else {
                        return Err(RowSchemaError::MissingField {
                            column: column.to_owned(),
                            field: field.name.clone(),
                        });
                    };
                    if matches!(wire, GqlWireValue::Null) && !field.nullable {
                        return Err(RowSchemaError::NullField {
                            column: column.to_owned(),
                            field: field.name.clone(),
                        });
                    }
                    validate_value(wire, &field.semantic_type, column, Some(&field.name))?;
                }
                Ok(())
            }
            other => Err(type_mismatch(other, ty, column, field)),
        },
        scalar => {
            if wire_tag_matches(value, scalar) {
                Ok(())
            } else {
                Err(type_mismatch(value, ty, column, field))
            }
        }
    }
}

fn type_mismatch(
    value: &GqlWireValue,
    ty: &SemanticType,
    column: &str,
    field: Option<&str>,
) -> RowSchemaError {
    let (declared, received) = (semantic_type_tag(ty), wire_tag(value));
    match field {
        Some(field) => RowSchemaError::FieldTypeMismatch {
            column: column.to_owned(),
            field: field.to_owned(),
            declared: declared.to_owned(),
            received: received.to_owned(),
        },
        None => RowSchemaError::TypeMismatch {
            column: column.to_owned(),
            declared: declared.to_owned(),
            received: received.to_owned(),
        },
    }
}

/// The wire tag a declared semantic type must arrive as (scalars; `List`/`Record`/`Path`
/// recurse in [`validate_value`]).
fn wire_tag_matches(value: &GqlWireValue, ty: &SemanticType) -> bool {
    semantic_type_tag(ty) == wire_tag(value)
}

fn semantic_type_tag(ty: &SemanticType) -> &'static str {
    match ty {
        SemanticType::Null => "Null",
        SemanticType::Bool => "Bool",
        SemanticType::Int8 => "Int8",
        SemanticType::Int16 => "Int16",
        SemanticType::Int32 => "Int32",
        SemanticType::Int64 => "Int64",
        SemanticType::Int128 => "Int128",
        SemanticType::Int256 => "Int256",
        SemanticType::Uint8 => "Uint8",
        SemanticType::Uint16 => "Uint16",
        SemanticType::Uint32 => "Uint32",
        SemanticType::Uint64 => "Uint64",
        SemanticType::Uint128 => "Uint128",
        SemanticType::Uint256 => "Uint256",
        SemanticType::Float16 => "Float16",
        SemanticType::Float32 => "Float32",
        SemanticType::Float64 => "Float64",
        SemanticType::Float128 => "Float128",
        SemanticType::Float256 => "Float256",
        SemanticType::Decimal => "Decimal",
        SemanticType::Text => "Text",
        SemanticType::Bytes => "Bytes",
        SemanticType::Date => "Date",
        SemanticType::Time => "Time",
        SemanticType::LocalTime => "LocalTime",
        SemanticType::DateTime => "DateTime",
        SemanticType::LocalDateTime => "LocalDateTime",
        SemanticType::ZonedDateTime => "ZonedDateTime",
        SemanticType::ZonedTime => "ZonedTime",
        SemanticType::Duration => "Duration",
        SemanticType::Principal => "Principal",
        SemanticType::List { .. } => "List",
        SemanticType::Record { .. } => "Record",
        SemanticType::Path => "Path",
    }
}

fn wire_tag(value: &GqlWireValue) -> &'static str {
    match value {
        GqlWireValue::Null => "Null",
        GqlWireValue::Bool(_) => "Bool",
        GqlWireValue::Int8(_) => "Int8",
        GqlWireValue::Int16(_) => "Int16",
        GqlWireValue::Int32(_) => "Int32",
        GqlWireValue::Int64(_) => "Int64",
        GqlWireValue::Int128(_) => "Int128",
        GqlWireValue::Int256(_) => "Int256",
        GqlWireValue::Uint8(_) => "Uint8",
        GqlWireValue::Uint16(_) => "Uint16",
        GqlWireValue::Uint32(_) => "Uint32",
        GqlWireValue::Uint64(_) => "Uint64",
        GqlWireValue::Uint128(_) => "Uint128",
        GqlWireValue::Uint256(_) => "Uint256",
        GqlWireValue::Float16(_) => "Float16",
        GqlWireValue::Float32(_) => "Float32",
        GqlWireValue::Float64(_) => "Float64",
        GqlWireValue::Float128(_) => "Float128",
        GqlWireValue::Float256(_) => "Float256",
        GqlWireValue::Decimal(_) => "Decimal",
        GqlWireValue::Text(_) => "Text",
        GqlWireValue::Bytes(_) => "Bytes",
        GqlWireValue::Date(_) => "Date",
        GqlWireValue::Time(_) => "Time",
        GqlWireValue::LocalTime(_) => "LocalTime",
        GqlWireValue::DateTime { .. } => "DateTime",
        GqlWireValue::LocalDateTime { .. } => "LocalDateTime",
        GqlWireValue::ZonedDateTime { .. } => "ZonedDateTime",
        GqlWireValue::ZonedTime { .. } => "ZonedTime",
        GqlWireValue::Duration { .. } => "Duration",
        GqlWireValue::Principal(_) => "Principal",
        GqlWireValue::ExtensionLeaf { .. } => "ExtensionLeaf",
        GqlWireValue::ValueBinary(_) => "ValueBinary",
        GqlWireValue::List(_) => "List",
        GqlWireValue::Path(_) => "Path",
        GqlWireValue::Record(_) => "Record",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_prepared_api::Column;

    fn column(name: &str, semantic_type: SemanticType, nullable: bool) -> Column {
        Column {
            name: name.to_owned(),
            semantic_type,
            nullable,
        }
    }

    fn row(columns: Vec<(&str, GqlWireValue)>) -> GqlWireRow {
        GqlWireRow {
            columns: columns
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
        }
    }

    /// The demo regression: `cite_edge_id` is a hop-trail list; a scalar Bytes declaration
    /// must fail with both sides named (declared Bytes, received List).
    #[test]
    fn bytes_column_rejects_a_list_value_naming_both_sides() {
        let columns = [column(
            "cite_edge_id",
            SemanticType::Bytes,
            false,
        )];
        let value = GqlWireValue::List(vec![GqlWireValue::Bytes(vec![1])]);
        let error = validate_row(&row(vec![("cite_edge_id", value)]), &columns)
            .expect_err("List under a Bytes column must fail");
        assert_eq!(
            error,
            RowSchemaError::TypeMismatch {
                column: "cite_edge_id".to_owned(),
                declared: "Bytes".to_owned(),
                received: "List".to_owned(),
            }
        );
    }

    #[test]
    fn list_of_bytes_column_accepts_hop_trails() {
        let columns = [column(
            "cite_edge_id",
            SemanticType::List {
                element: Box::new(SemanticType::Bytes),
            },
            false,
        )];
        let value = GqlWireValue::List(vec![
            GqlWireValue::Bytes(vec![1, 2]),
            GqlWireValue::Bytes(vec![3]),
        ]);
        validate_row(&row(vec![("cite_edge_id", value)]), &columns).expect("hop trail passes");
    }

    #[test]
    fn null_is_enforced_against_declared_nullability() {
        let columns = [
            column("maybe", SemanticType::Bytes, true),
            column("required", SemanticType::Bytes, false),
        ];
        validate_row(
            &row(vec![
                ("maybe", GqlWireValue::Null),
                ("required", GqlWireValue::Bytes(vec![])),
            ]),
            &columns,
        )
        .expect("nullable null passes");
        let error = validate_row(
            &row(vec![
                ("maybe", GqlWireValue::Bytes(vec![])),
                ("required", GqlWireValue::Null),
            ]),
            &columns,
        )
        .expect_err("non-nullable null must fail");
        assert_eq!(
            error,
            RowSchemaError::NullColumn {
                column: "required".to_owned(),
            }
        );
    }

    #[test]
    fn missing_column_is_rejected() {
        let columns = [column("present", SemanticType::Text, false)];
        let error = validate_row(
            &row(vec![("other", GqlWireValue::Text("x".to_owned()))]),
            &columns,
        )
        .expect_err("missing column must fail");
        assert_eq!(
            error,
            RowSchemaError::MissingColumn {
                column: "present".to_owned(),
            }
        );
    }

    #[test]
    fn list_element_type_drift_is_rejected() {
        let columns = [column(
            "ids",
            SemanticType::List {
                element: Box::new(SemanticType::Bytes),
            },
            false,
        )];
        let value = GqlWireValue::List(vec![
            GqlWireValue::Bytes(vec![1]),
            GqlWireValue::Int64(7),
        ]);
        let error = validate_row(&row(vec![("ids", value)]), &columns)
            .expect_err("element drift must fail");
        assert_eq!(
            error,
            RowSchemaError::TypeMismatch {
                column: "ids".to_owned(),
                declared: "Bytes".to_owned(),
                received: "Int64".to_owned(),
            }
        );
    }

    #[test]
    fn record_fields_validate_recursively() {
        let columns = [column(
            "hit",
            SemanticType::Record {
                fields: vec![RecordField {
                    name: "score".to_owned(),
                    semantic_type: SemanticType::Float64,
                    nullable: false,
                }],
            },
            false,
        )];

        // Missing field.
        let error = validate_row(
            &row(vec![("hit", GqlWireValue::Record(vec![]))]),
            &columns,
        )
        .expect_err("missing record field must fail");
        assert!(matches!(error, RowSchemaError::MissingField { .. }));

        // Field type drift.
        let error = validate_row(
            &row(vec![(
                "hit",
                GqlWireValue::Record(vec![(
                    "score".to_owned(),
                    GqlWireValue::Int64(9),
                )]),
            )]),
            &columns,
        )
        .expect_err("field type drift must fail");
        assert_eq!(
            error,
            RowSchemaError::FieldTypeMismatch {
                column: "hit".to_owned(),
                field: "score".to_owned(),
                declared: "Float64".to_owned(),
                received: "Int64".to_owned(),
            }
        );
    }

    #[test]
    fn the_citation_reach_column_set_passes_end_to_end() {
        // Byte-identical shape to the live demo manifest after the group-variable typing
        // fix: document_id Bytes, title Text, cite_edge_id List(Bytes).
        let columns = [
            column("document_id", SemanticType::Bytes, false),
            column("title", SemanticType::Text, false),
            column(
                "cite_edge_id",
                SemanticType::List {
                    element: Box::new(SemanticType::Bytes),
                },
                false,
            ),
        ];
        validate_row(
            &row(vec![
                ("document_id", GqlWireValue::Bytes(vec![9, 9])),
                ("title", GqlWireValue::Text("The GQL standard".to_owned())),
                (
                    "cite_edge_id",
                    GqlWireValue::List(vec![
                        GqlWireValue::Bytes(vec![1]),
                        GqlWireValue::Bytes(vec![2, 3]),
                    ]),
                ),
            ]),
            &columns,
        )
        .expect("the demo row shape validates");
    }
}