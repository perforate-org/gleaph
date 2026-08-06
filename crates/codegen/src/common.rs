//! Shared renderer helpers for semantic-value encoding, naming, and literal emission.
//!
//! This module contains syntax-neutral mechanics used by multiple language profiles; profile
//! policy and output structure remain in the language-specific modules.

use crate::{Column, RecordField, SemanticType};

/// The `ApiValueHint` name for a leaf semantic type (matches the SDK's wire hints).
fn semantic_hint(semantic_type: &SemanticType) -> &'static str {
    match semantic_type {
        SemanticType::Null => "Null",
        SemanticType::Bool => "Bool",
        SemanticType::Int8 => "Int8",
        SemanticType::Int16 => "Int16",
        SemanticType::Int32 => "Int32",
        SemanticType::Int64 => "Int64",
        SemanticType::Uint8 => "Uint8",
        SemanticType::Uint16 => "Uint16",
        SemanticType::Uint32 => "Uint32",
        SemanticType::Uint64 => "Uint64",
        SemanticType::Int128 => "Int128",
        SemanticType::Uint128 => "Uint128",
        SemanticType::Int256 => "Int256",
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
        SemanticType::List { .. } | SemanticType::Record { .. } => {
            // Unreachable: containers recurse in [`encode_expression`].
            "List"
        }
        SemanticType::Path => "Path",
    }
}

/// Render an expression that converts a user value into the wire [`ApiValue`].
///
/// Generated code passes the semantic type as an `ApiValueHint` so the SDK picks the exact wire
/// variant; list and record elements recurse so nested exotic values keep their element hints.
pub(crate) fn encode_expression(access: &str, semantic_type: &SemanticType) -> String {
    match semantic_type {
        SemanticType::List { element } => format!(
            "{{ List: {access}.map((value) => {}) }}",
            encode_expression("value", element)
        ),
        SemanticType::Record { fields } => {
            let fields = fields
                .iter()
                .map(|field| {
                    let key = json_string(&field.name);
                    let value =
                        encode_expression(&format!("{access}[{key}]"), &field.semantic_type);
                    format!("[{key}, {value}]")
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ Record: Object.fromEntries([{fields}]) }}")
        }
        _ => format!("toApiValue({access}, \"{}\")", semantic_hint(semantic_type)),
    }
}

pub(crate) fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

pub(crate) fn pascal_case(name: &str) -> String {
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

pub(crate) fn ts_property(name: &str) -> String {
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

/// Lower-camel-case method identifier derived from a wire name ("find-users" -> "findUsers").
///
/// Derived from [`pascal_case`] so identifier collisions are identical across profiles: two wire
/// names collide as camel-case iff they collide as Pascal-case.
pub(crate) fn camel_case(name: &str) -> String {
    let pascal = pascal_case(name);
    let mut chars = pascal.chars();
    match chars.next() {
        Some(first) => {
            let mut result = first.to_ascii_lowercase().to_string();
            result.push_str(chars.as_str());
            result
        }
        None => String::new(),
    }
}

/// SCREAMING_SNAKE_CASE identifier from a wire name ("find-users" -> "FIND_USERS").
pub(crate) fn screaming_snake_case(name: &str) -> String {
    let mut result = String::new();
    let mut prev_lower_or_digit = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && prev_lower_or_digit {
                result.push('_');
            }
            result.push(ch.to_ascii_uppercase());
            prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            result.push('_');
            prev_lower_or_digit = false;
        }
    }
    result
}

/// Module-level const name holding the column specs of one prepared operation.
pub(crate) fn column_const_name(wire_name: &str) -> String {
    format!("{}_COLUMNS", screaming_snake_case(wire_name))
}

/// Render one `SemanticType` as a TypeScript literal for a `PreparedManifestColumn`
/// (`semantic_type` field of the SDK's discriminated union).
pub(crate) fn ts_semantic_type_literal(semantic_type: &SemanticType) -> String {
    match semantic_type {
        SemanticType::List { element } => format!(
            "{{ List: {{ element: {} }} }}",
            ts_semantic_type_literal(element)
        ),
        SemanticType::Record { fields } => {
            let fields = fields
                .iter()
                .map(ts_record_field_literal)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ Record: {{ fields: [{fields}] }} }}")
        }
        _ => format!("\"{}\"", semantic_hint(semantic_type)),
    }
}

fn ts_record_field_literal(field: &RecordField) -> String {
    format!(
        "{{ name: {}, semantic_type: {}, nullable: {} }}",
        json_string(&field.name),
        ts_semantic_type_literal(&field.semantic_type),
        field.nullable
    )
}

/// Render a `PreparedManifestColumn[]` literal for a result schema.
pub(crate) fn ts_column_specs(columns: &[Column]) -> String {
    let columns = columns
        .iter()
        .map(|column| {
            format!(
                "{{ name: {}, semantic_type: {}, nullable: {} }}",
                json_string(&column.name),
                ts_semantic_type_literal(&column.semantic_type),
                column.nullable
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{columns}]")
}

pub(crate) fn append_line_doc(out: &mut String, indent: &str, prefix: &str, text: &str) {
    for line in text.lines() {
        out.push_str(indent);
        out.push_str(prefix);
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
}

pub(crate) fn append_jsdoc(out: &mut String, indent: &str, text: &str) {
    out.push_str(indent);
    out.push_str("/**\n");
    for line in text.lines() {
        out.push_str(indent);
        out.push_str(" * ");
        out.push_str(&line.replace("*/", "* /"));
        out.push('\n');
    }
    out.push_str(indent);
    out.push_str(" */\n");
}
