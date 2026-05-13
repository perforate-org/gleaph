//! Identifier length limits for catalog names (UTF-8 byte length after encoding).
//!
//! These bounds keep stable structures and on-disk encodings predictable and
//! make prefix-range scans (`StableBTreeMap::range`) workable when paired with
//! [`crate::name_limits::lexicographic_successor_within_max_bytes`].

use std::fmt;

/// Maximum UTF-8 byte length for a property key name (node or edge property).
pub const MAX_PROPERTY_NAME_BYTES: usize = 1024;

/// Maximum UTF-8 byte length for a node label or edge relationship type label.
pub const MAX_LABEL_NAME_BYTES: usize = 512;

/// Maximum UTF-8 byte length for graph-type identifiers: node type name,
/// node type alias, edge type name, and endpoint references in DDL.
pub const MAX_GRAPH_TYPE_IDENTIFIER_BYTES: usize = 256;

/// Maximum UTF-8 byte length for one segment of a catalog [`crate::ast::ObjectName`]
/// (schema name, graph name, `SESSION SET SCHEMA`, `MATCH … USE`, etc.).
pub const MAX_CATALOG_NAME_PART_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NameLimitKind {
    PropertyName,
    LabelName,
    GraphTypeIdentifier,
    CatalogNamePart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameLimitError {
    pub kind: NameLimitKind,
    pub len: usize,
    pub max: usize,
}

impl fmt::Display for NameLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self.kind {
            NameLimitKind::PropertyName => "property name",
            NameLimitKind::LabelName => "label name",
            NameLimitKind::GraphTypeIdentifier => "graph type identifier",
            NameLimitKind::CatalogNamePart => "catalog name segment",
        };
        write!(
            f,
            "{label} is {} bytes; maximum allowed is {} bytes",
            self.len, self.max
        )
    }
}

impl std::error::Error for NameLimitError {}

fn validate_len(kind: NameLimitKind, name: &str, max: usize) -> Result<(), NameLimitError> {
    let len = name.len();
    if len > max {
        return Err(NameLimitError { kind, len, max });
    }
    Ok(())
}

/// Validates a runtime or schema property key name.
pub fn validate_property_name(name: &str) -> Result<(), NameLimitError> {
    validate_len(NameLimitKind::PropertyName, name, MAX_PROPERTY_NAME_BYTES)
}

/// Validates a node label or edge label string.
pub fn validate_label_name(name: &str) -> Result<(), NameLimitError> {
    validate_len(NameLimitKind::LabelName, name, MAX_LABEL_NAME_BYTES)
}

/// Validates graph-type DDL identifiers (type names, aliases, endpoint refs).
pub fn validate_graph_type_identifier(name: &str) -> Result<(), NameLimitError> {
    validate_len(
        NameLimitKind::GraphTypeIdentifier,
        name,
        MAX_GRAPH_TYPE_IDENTIFIER_BYTES,
    )
}

/// Validates one [`crate::ast::ObjectName`] path segment (schema, graph, etc.).
pub fn validate_catalog_name_part(name: &str) -> Result<(), NameLimitError> {
    validate_len(
        NameLimitKind::CatalogNamePart,
        name,
        MAX_CATALOG_NAME_PART_BYTES,
    )
}

/// Returns the smallest string `t` such that `t > s` in Rust `str` ordering
/// and `t.len() <= max_bytes`, or `None` if no such string exists.
///
/// Used to build an **exclusive** end key for
/// `StableBTreeMap::range` when keys are ordered as `(entity_kind, property_name, …)`
/// and all `property_name` values respect `max_bytes`.
pub fn lexicographic_successor_within_max_bytes(s: &str, max_bytes: usize) -> Option<String> {
    if s.is_empty() {
        return (max_bytes > 0).then(|| "\0".to_string());
    }
    let mut chars: Vec<char> = s.chars().collect();
    while let Some(last) = chars.pop() {
        let mut code = last as u32;
        while code < 0x10FFFF {
            code += 1;
            let Some(c) = char::from_u32(code) else {
                continue;
            };
            chars.push(c);
            let candidate: String = chars.iter().collect();
            if candidate.len() <= max_bytes && candidate.as_str() > s {
                return Some(candidate);
            }
            chars.pop();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successor_foo_is_fop() {
        let s = lexicographic_successor_within_max_bytes("foo", MAX_PROPERTY_NAME_BYTES).unwrap();
        assert_eq!(s, "fop");
    }

    #[test]
    fn successor_none_when_max_too_small() {
        // The successor of `""` with length budget `0` is nonempty, so impossible.
        assert!(lexicographic_successor_within_max_bytes("", 0).is_none());
    }

    #[test]
    fn successor_empty_string_is_nul_when_budget_allows() {
        let s = lexicographic_successor_within_max_bytes("", 1).unwrap();
        assert_eq!(s, "\0");
    }
}
