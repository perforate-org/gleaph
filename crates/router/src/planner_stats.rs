//! Planner [`GraphStats`] adapter over stable `(graph, kind, property_id)` membership.
//!
//! Indexed properties are stored as [`PropertyId`] sets loaded from stable memory.
//! [`GraphStats`] name-based queries resolve property names through the router property
//! catalog on demand (forward lookup only — no bulk reverse intern).

use std::collections::BTreeSet;

use gleaph_gql_planner::GraphStats;
use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::index::IndexedPropertyKind;

use crate::facade::stable::ROUTER_PROPERTY_CATALOG;

/// One administrator-defined index (ADR 0009 §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCatalogEntry {
    pub kind: IndexedPropertyKind,
    pub vertex_label: Option<String>,
    pub edge_label: Option<String>,
    pub property: String,
}

/// Per-graph indexed property membership for cost-based planning.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouterGraphStats {
    vertex_property_ids: BTreeSet<PropertyId>,
    edge_property_ids: BTreeSet<PropertyId>,
}

impl RouterGraphStats {
    pub(crate) fn from_property_ids(
        vertex_property_ids: BTreeSet<PropertyId>,
        edge_property_ids: BTreeSet<PropertyId>,
    ) -> Self {
        Self {
            vertex_property_ids,
            edge_property_ids,
        }
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
                .get_id(property)
                .is_some_and(|property_id| self.is_property_id_indexed(kind, property_id))
        })
    }

    #[cfg(test)]
    pub fn test_vertex_indexed(
        store: &crate::facade::store::RouterStore,
        properties: &[&str],
    ) -> Self {
        let vertex = properties
            .iter()
            .map(|property| store.lookup_property_id(property))
            .collect::<Result<BTreeSet<_>, _>>()
            .expect("test property interned");
        Self::from_property_ids(vertex, BTreeSet::new())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::store::RouterStore;
    use crate::init::RouterInitArgs;
    use candid::Principal;

    #[test]
    fn from_property_ids_tracks_membership_by_id() {
        let stats = RouterGraphStats::from_property_ids(
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
            controllers: vec![],
        });
        store.bootstrap_controllers(&[admin]);
        let property_id = store
            .admin_intern_property(admin, "region")
            .expect("intern region");
        let stats = RouterGraphStats::from_property_ids(
            [property_id].into_iter().collect(),
            BTreeSet::new(),
        );
        assert!(stats.is_vertex_property_indexed("region"));
        assert!(!stats.is_vertex_property_indexed("missing"));
    }
}
