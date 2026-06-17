//! Planner [`GraphStats`] adapter over stable `(graph, kind, property_id)` membership.
//!
//! Indexed properties are stored as [`PropertyId`] sets loaded from stable memory.
//! Edge indexes additionally load `(label_id, property_id, direction)` from named-index records (ADR 0012).

use std::collections::BTreeSet;

use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::GraphStats;
use gleaph_graph_kernel::entry::{GraphId, PropertyId};
use gleaph_graph_kernel::index::IndexedPropertyKind;

use crate::edge_index_direction::{
    EdgeIndexDirectionTag, index_applies_to_query, tag_to_direction,
};
use crate::facade::stable::ROUTER_PROPERTY_CATALOG;
use crate::facade::store::RouterStore;

/// One administrator-defined index (ADR 0009 §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCatalogEntry {
    pub kind: IndexedPropertyKind,
    pub vertex_label: Option<String>,
    pub edge_label: Option<String>,
    pub property: String,
    pub edge_direction: Option<EdgeDirection>,
}

/// Semantic identity of an edge property index (ADR 0012).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeIndexMembership {
    pub property_id: PropertyId,
    pub label_id: u16,
    pub direction: EdgeIndexDirectionTag,
}

/// Per-graph indexed property membership for cost-based planning.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouterGraphStats {
    graph_id: GraphId,
    vertex_property_ids: BTreeSet<PropertyId>,
    edge_property_ids: BTreeSet<PropertyId>,
    edge_indexes: BTreeSet<EdgeIndexMembership>,
}

impl RouterGraphStats {
    pub(crate) fn graph_id(&self) -> GraphId {
        self.graph_id
    }

    pub(crate) fn from_catalog(
        graph_id: GraphId,
        vertex_property_ids: BTreeSet<PropertyId>,
        edge_property_ids: BTreeSet<PropertyId>,
        edge_indexes: BTreeSet<EdgeIndexMembership>,
    ) -> Self {
        Self {
            graph_id,
            vertex_property_ids,
            edge_property_ids,
            edge_indexes,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_property_ids(
        graph_id: GraphId,
        vertex_property_ids: BTreeSet<PropertyId>,
        edge_property_ids: BTreeSet<PropertyId>,
    ) -> Self {
        Self::from_catalog(
            graph_id,
            vertex_property_ids,
            edge_property_ids,
            BTreeSet::new(),
        )
    }

    pub(crate) fn is_vertex_property_id_indexed(&self, property_id: PropertyId) -> bool {
        self.vertex_property_ids.contains(&property_id)
    }

    pub(crate) fn is_edge_property_id_indexed(&self, property_id: PropertyId) -> bool {
        self.edge_property_ids.contains(&property_id)
    }

    fn is_property_id_indexed(&self, kind: IndexedPropertyKind, property_id: PropertyId) -> bool {
        match kind {
            IndexedPropertyKind::Vertex => self.is_vertex_property_id_indexed(property_id),
            IndexedPropertyKind::Edge => self.is_edge_property_id_indexed(property_id),
        }
    }

    fn is_named_property_indexed(&self, kind: IndexedPropertyKind, property: &str) -> bool {
        ROUTER_PROPERTY_CATALOG.with_borrow(|catalog| {
            catalog
                .get_id(self.graph_id, property)
                .is_some_and(|property_id| self.is_property_id_indexed(kind, property_id))
        })
    }

    fn is_edge_indexed_for(
        &self,
        label: &str,
        property: &str,
        query_direction: EdgeDirection,
    ) -> bool {
        ROUTER_PROPERTY_CATALOG.with_borrow(|catalog| {
            let Some(property_id) = catalog.get_id(self.graph_id, property) else {
                return false;
            };
            let label_id = match RouterStore::new().lookup_edge_label_id(self.graph_id, label) {
                Ok(id) => id.raw(),
                Err(_) => return false,
            };
            self.edge_indexes.iter().any(|entry| {
                entry.property_id == property_id
                    && entry.label_id == label_id
                    && index_applies_to_query(tag_to_direction(entry.direction), query_direction)
            })
        })
    }

    #[cfg(test)]
    pub fn test_vertex_indexed(
        graph_id: GraphId,
        store: &RouterStore,
        properties: &[&str],
    ) -> Self {
        let vertex = properties
            .iter()
            .map(|property| store.lookup_property_id(graph_id, property))
            .collect::<Result<BTreeSet<_>, _>>()
            .expect("test property interned");
        Self::from_property_ids(graph_id, vertex, BTreeSet::new())
    }
}

impl GraphStats for RouterGraphStats {
    fn is_vertex_property_indexed(&self, property: &str) -> bool {
        self.is_named_property_indexed(IndexedPropertyKind::Vertex, property)
    }

    fn is_vertex_property_range_indexed(&self, _property: &str) -> bool {
        false
    }

    fn is_edge_property_indexed(&self, property: &str) -> bool {
        self.is_named_property_indexed(IndexedPropertyKind::Edge, property)
    }

    fn is_edge_property_indexed_for(
        &self,
        label: Option<&str>,
        property: &str,
        direction: EdgeDirection,
    ) -> bool {
        match label {
            Some(label) => self.is_edge_indexed_for(label, property, direction),
            None => self.is_edge_property_indexed(property),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge_index_direction::EdgeIndexDirectionTag;
    use crate::facade::store::RouterStore;
    use crate::init::RouterInitArgs;
    use candid::Principal;

    use crate::facade::store::catalog_test_support::GRAPH as TEST_GRAPH;

    #[test]
    fn from_property_ids_tracks_membership_by_id() {
        let stats = RouterGraphStats::from_property_ids(
            GraphId::from_raw(1),
            [PropertyId::from_raw(1)].into_iter().collect(),
            [PropertyId::from_raw(2)].into_iter().collect(),
        );
        assert!(stats.is_vertex_property_id_indexed(PropertyId::from_raw(1)));
        assert!(stats.is_edge_property_id_indexed(PropertyId::from_raw(2)));
        assert!(!stats.is_vertex_property_id_indexed(PropertyId::from_raw(2)));
    }

    #[test]
    fn adapter_resolves_property_name_via_catalog() {
        let store = RouterStore::new();
        let admin = Principal::anonymous();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
        });
        crate::facade::auth::grant_admins(&[admin]);
        crate::facade::store::catalog_test_support::register_graph(&store, admin, TEST_GRAPH);
        let graph_id = store.resolve_graph_id(TEST_GRAPH).expect("test graph");
        let property_id = store
            .admin_intern_property(admin, TEST_GRAPH, "region")
            .expect("intern region");
        let stats = RouterGraphStats::from_property_ids(
            graph_id,
            [property_id].into_iter().collect(),
            BTreeSet::new(),
        );
        assert!(stats.is_vertex_property_indexed("region"));
        assert!(!stats.is_vertex_property_indexed("missing"));
    }

    #[test]
    fn edge_index_subset_rule_uses_direction() {
        let store = RouterStore::new();
        let admin = Principal::anonymous();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
        });
        crate::facade::auth::grant_admins(&[admin]);
        crate::facade::store::catalog_test_support::register_graph(&store, admin, TEST_GRAPH);
        let graph_id = store.resolve_graph_id(TEST_GRAPH).expect("test graph");
        let _ = store
            .admin_intern_edge_label(admin, TEST_GRAPH, "KNOWS")
            .expect("label");
        let property_id = store
            .admin_intern_property(admin, TEST_GRAPH, "weight")
            .expect("property");
        let stats = RouterGraphStats::from_catalog(
            graph_id,
            BTreeSet::new(),
            [property_id].into_iter().collect(),
            [EdgeIndexMembership {
                property_id,
                label_id: 1,
                direction: EdgeIndexDirectionTag::PointingRight,
            }]
            .into_iter()
            .collect(),
        );
        assert!(stats.is_edge_property_indexed_for(
            Some("KNOWS"),
            "weight",
            EdgeDirection::PointingRight,
        ));
        assert!(!stats.is_edge_property_indexed_for(
            Some("KNOWS"),
            "weight",
            EdgeDirection::Undirected,
        ));
    }
}
