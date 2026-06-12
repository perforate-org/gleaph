//! Planner [`GraphStats`] backed by router stable catalog.

use std::collections::{BTreeMap, BTreeSet};

use gleaph_gql_planner::GraphStats;
use gleaph_graph_kernel::index::IndexedPropertyKind;

use crate::facade::stable::ROUTER_INDEXED_PROPERTIES;
use crate::state::RouterError;

/// One administrator-defined index (ADR 0009 §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCatalogEntry {
    pub kind: IndexedPropertyKind,
    pub vertex_label: Option<String>,
    pub edge_label: Option<String>,
    pub property: String,
}

/// Per-graph indexed property catalog for cost-based planning.
#[derive(Clone, Debug, Default)]
pub struct RouterGraphStats {
    indexed_vertex_properties: BTreeSet<String>,
    range_indexed_vertex_properties: BTreeSet<String>,
    indexed_edge_properties: BTreeSet<String>,
    named_indexes: BTreeMap<String, IndexCatalogEntry>,
}

impl RouterGraphStats {
    pub fn for_graph(logical_graph_name: &str) -> Self {
        ROUTER_INDEXED_PROPERTIES.with_borrow(|m| {
            m.get(&logical_graph_name.to_string())
                .cloned()
                .unwrap_or_default()
        })
    }

    pub fn with_indexed_vertex_property(mut self, property: impl Into<String>) -> Self {
        self.indexed_vertex_properties.insert(property.into());
        self
    }

    pub fn with_indexed_edge_property(mut self, property: impl Into<String>) -> Self {
        self.indexed_edge_properties.insert(property.into());
        self
    }

    /// Insert a DDL-defined index. Returns `Ok(false)` when `if_not_exists` and name taken.
    pub fn create_named_index(
        &mut self,
        index_name: &str,
        entry: IndexCatalogEntry,
        if_not_exists: bool,
    ) -> Result<bool, RouterError> {
        if self.named_indexes.contains_key(index_name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(RouterError::Conflict(format!(
                "index already exists: {index_name}"
            )));
        }
        match entry.kind {
            IndexedPropertyKind::Vertex => {
                self.indexed_vertex_properties
                    .insert(entry.property.clone());
            }
            IndexedPropertyKind::Edge => {
                self.indexed_edge_properties.insert(entry.property.clone());
            }
        }
        self.named_indexes.insert(index_name.to_string(), entry);
        Ok(true)
    }

    /// Remove a DDL-defined index. Returns `Ok(None)` when `if_exists` and missing.
    pub fn drop_named_index(
        &mut self,
        index_name: &str,
        if_exists: bool,
    ) -> Result<Option<(IndexedPropertyKind, String)>, RouterError> {
        let Some(entry) = self.named_indexes.remove(index_name) else {
            if if_exists {
                return Ok(None);
            }
            return Err(RouterError::NotFound(index_name.to_string()));
        };
        let kind = entry.kind;
        let property = entry.property.clone();
        let still_used = self
            .named_indexes
            .values()
            .any(|e| e.kind == kind && e.property == property);
        if !still_used {
            match kind {
                IndexedPropertyKind::Vertex => {
                    self.indexed_vertex_properties.remove(&property);
                }
                IndexedPropertyKind::Edge => {
                    self.indexed_edge_properties.remove(&property);
                }
            }
        }
        Ok(Some((kind, property)))
    }

    pub fn is_property_registered(&self, kind: IndexedPropertyKind, property: &str) -> bool {
        match kind {
            IndexedPropertyKind::Vertex => self.indexed_vertex_properties.contains(property),
            IndexedPropertyKind::Edge => self.indexed_edge_properties.contains(property),
        }
    }
}

impl GraphStats for RouterGraphStats {
    fn is_vertex_property_indexed(&self, property: &str) -> bool {
        self.indexed_vertex_properties.contains(property)
    }

    fn is_vertex_property_range_indexed(&self, property: &str) -> bool {
        self.range_indexed_vertex_properties.contains(property)
    }

    fn is_edge_property_indexed(&self, property: &str) -> bool {
        self.indexed_edge_properties.contains(property)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::index::IndexedPropertyKind;

    #[test]
    fn named_index_create_and_drop_updates_property_sets() {
        let mut stats = RouterGraphStats::default();
        let entry = IndexCatalogEntry {
            kind: IndexedPropertyKind::Vertex,
            vertex_label: Some("Person".into()),
            edge_label: None,
            property: "age".into(),
        };
        assert!(
            stats
                .create_named_index("person_age", entry, false)
                .expect("create")
        );
        assert!(stats.is_vertex_property_indexed("age"));
        let removed = stats
            .drop_named_index("person_age", false)
            .expect("drop")
            .expect("removed");
        assert_eq!(removed, (IndexedPropertyKind::Vertex, "age".to_string()));
        assert!(!stats.is_vertex_property_indexed("age"));
    }
}
