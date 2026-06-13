//! Planner [`GraphStats`] view built from stable index catalog rows.

use std::collections::BTreeSet;

use gleaph_gql_planner::GraphStats;
use gleaph_graph_kernel::index::IndexedPropertyKind;

/// One administrator-defined index (ADR 0009 §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCatalogEntry {
    pub kind: IndexedPropertyKind,
    pub vertex_label: Option<String>,
    pub edge_label: Option<String>,
    pub property: String,
}

/// Per-graph indexed property catalog for cost-based planning.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouterGraphStats {
    indexed_vertex_properties: BTreeSet<String>,
    range_indexed_vertex_properties: BTreeSet<String>,
    indexed_edge_properties: BTreeSet<String>,
}

impl RouterGraphStats {
    pub(crate) fn from_indexed_properties(
        indexed_vertex_properties: BTreeSet<String>,
        indexed_edge_properties: BTreeSet<String>,
    ) -> Self {
        Self {
            indexed_vertex_properties,
            range_indexed_vertex_properties: BTreeSet::new(),
            indexed_edge_properties,
        }
    }

    #[cfg(test)]
    pub fn test_vertex_indexed(properties: &[&str]) -> Self {
        Self::from_indexed_properties(
            properties
                .iter()
                .map(|property| (*property).to_string())
                .collect(),
            BTreeSet::new(),
        )
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

    #[test]
    fn from_indexed_properties_tracks_membership() {
        let stats = RouterGraphStats::from_indexed_properties(
            ["age".to_string()].into_iter().collect(),
            ["weight".to_string()].into_iter().collect(),
        );
        assert!(stats.is_vertex_property_indexed("age"));
        assert!(stats.is_edge_property_indexed("weight"));
        assert!(!stats.is_vertex_property_indexed("weight"));
    }

    #[test]
    fn test_vertex_indexed_helper_tracks_membership() {
        let stats = RouterGraphStats::test_vertex_indexed(&["region"]);
        assert!(stats.is_vertex_property_indexed("region"));
    }
}
