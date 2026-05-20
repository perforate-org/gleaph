//! Planner [`GraphStats`] backed by router stable catalog.

use std::collections::BTreeSet;

use gleaph_gql_planner::GraphStats;

use crate::facade::stable::ROUTER_INDEXED_PROPERTIES;

/// Per-graph indexed property catalog for cost-based planning.
#[derive(Clone, Debug, Default)]
pub struct RouterGraphStats {
    indexed_vertex_properties: BTreeSet<String>,
    range_indexed_vertex_properties: BTreeSet<String>,
    indexed_edge_properties: BTreeSet<String>,
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
