//! Shared renderer helpers for semantic-value encoding, naming, and literal emission.
//!
//! This module contains syntax-neutral mechanics used by multiple language profiles; profile
//! policy and output structure remain in the language-specific modules.

use crate::SemanticType;

pub(crate) fn encode_expression(access: &str, semantic_type: &SemanticType) -> String {
    match semantic_type {
        SemanticType::Null => "{ Null: null }".to_string(),
        SemanticType::Bool => format!("{{ Bool: {access} }}"),
        SemanticType::Int8 => format!("{{ Int8: {access} }}"),
        SemanticType::Int16 => format!("{{ Int16: {access} }}"),
        SemanticType::Int32 => format!("{{ Int32: {access} }}"),
        SemanticType::Int64 => format!("{{ Int64: {access} }}"),
        SemanticType::Uint8 => format!("{{ Uint8: {access} }}"),
        SemanticType::Uint16 => format!("{{ Uint16: {access} }}"),
        SemanticType::Uint32 => format!("{{ Uint32: {access} }}"),
        SemanticType::Uint64 => format!("{{ Uint64: {access} }}"),
        SemanticType::Int128 => format!("{{ Int128: {access} }}"),
        SemanticType::Uint128 => format!("{{ Uint128: {access} }}"),
        SemanticType::Int256 => format!("{{ Int256: {access} }}"),
        SemanticType::Uint256 => format!("{{ Uint256: {access} }}"),
        SemanticType::Float16 => format!("{{ Float16: {access} }}"),
        SemanticType::Float32 => format!("{{ Float32: {access} }}"),
        SemanticType::Float64 => format!("{{ Float64: {access} }}"),
        SemanticType::Float128 => format!("{{ Float128: {access} }}"),
        SemanticType::Float256 => format!("{{ Float256: {access} }}"),
        SemanticType::Decimal => format!("{{ Decimal: {access} }}"),
        SemanticType::Text => format!("{{ Text: {access} }}"),
        SemanticType::Bytes => format!("{{ Bytes: {access} }}"),
        SemanticType::Date => format!("{{ Date: {access} }}"),
        SemanticType::Time => format!("{{ Time: {access} }}"),
        SemanticType::LocalTime => format!("{{ LocalTime: {access} }}"),
        SemanticType::DateTime => format!("{{ DateTime: {access} }}"),
        SemanticType::LocalDateTime => format!("{{ LocalDateTime: {access} }}"),
        SemanticType::ZonedDateTime => format!("{{ ZonedDateTime: {access} }}"),
        SemanticType::ZonedTime => format!("{{ ZonedTime: {access} }}"),
        SemanticType::Duration => format!("{{ Duration: {access} }}"),
        SemanticType::Principal => format!("{{ Principal: {access} }}"),
        SemanticType::List { element } => format!(
            "{{ List: {access}.map((value) => {}) }}",
            encode_expression("value", element)
        ),
        SemanticType::Path => format!("{{ Path: {access} }}"),
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
