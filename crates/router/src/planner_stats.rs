//! Planner [`GraphStats`] adapter over stable `(graph, kind, property_id)` membership.
//!
//! Indexed properties are stored as [`PropertyId`] sets loaded from stable memory.
//! Edge indexes additionally load `(label_id, property_id, direction)` from named-index records (ADR 0012).

use std::collections::BTreeSet;

use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::GraphStats;
use gleaph_graph_kernel::entry::{GraphId, PropertyId};
use gleaph_graph_kernel::index::{
    EdgeIndexDirection, IndexMaintenancePhase, IndexedPropertyKind, PhysicalIndexId,
};

use crate::edge_index_direction::index_applies_to_query;
use crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES;
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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeIndexMembership {
    pub physical_index_id: PhysicalIndexId,
    pub catalog_epoch: u64,
    pub property_id: PropertyId,
    pub label_id: u16,
    pub direction: EdgeIndexDirection,
    pub field_path: String,
}

/// One active vertex index namespace projected from the Router-owned catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VertexIndexMembership {
    pub physical_index_id: PhysicalIndexId,
    pub catalog_epoch: u64,
    pub property_id: PropertyId,
    pub label_id: u16,
}

/// Planner-only projection. The stable catalog constructs this from `Active` index lifecycle rows;
/// callers must not project Preparing, Building, Sealing, or Aborting definitions into it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouterGraphStats {
    graph_id: GraphId,
    vertex_property_ids: BTreeSet<PropertyId>,
    edge_property_ids: BTreeSet<PropertyId>,
    vertex_indexes: BTreeSet<VertexIndexMembership>,
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
            vertex_indexes: BTreeSet::new(),
            edge_indexes,
        }
    }

    pub(crate) fn from_catalog_with_indexes(
        graph_id: GraphId,
        vertex_property_ids: BTreeSet<PropertyId>,
        edge_property_ids: BTreeSet<PropertyId>,
        vertex_indexes: BTreeSet<VertexIndexMembership>,
        edge_indexes: BTreeSet<EdgeIndexMembership>,
    ) -> Self {
        Self {
            graph_id,
            vertex_property_ids,
            edge_property_ids,
            vertex_indexes,
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

    /// Export the active per-graph catalog in the wire form consumed by graph shards
    /// (ADR 0023 D1). Every membership carries its Router-allocated physical namespace;
    /// logical property sets are not sufficient to maintain postings.
    pub(crate) fn to_active_indexed_property_catalog(
        &self,
    ) -> gleaph_graph_kernel::index::IndexedPropertyCatalog {
        gleaph_graph_kernel::index::IndexedPropertyCatalog {
            vertex_indexes: self
                .vertex_indexes
                .iter()
                .map(|m| gleaph_graph_kernel::index::IndexedVertexMembership {
                    physical_index_id: m.physical_index_id,
                    catalog_epoch: m.catalog_epoch,
                    phase: IndexMaintenancePhase::Active,
                    property_id: m.property_id.raw(),
                    label_id: m.label_id,
                })
                .collect(),
            edge_indexes: self
                .edge_indexes
                .iter()
                .map(|m| gleaph_graph_kernel::index::IndexedEdgeMembership {
                    physical_index_id: m.physical_index_id,
                    catalog_epoch: m.catalog_epoch,
                    phase: IndexMaintenancePhase::Active,
                    label_id: m.label_id,
                    property_id: m.property_id.raw(),
                    direction: m.direction,
                    field_path: m.field_path.clone(),
                })
                .collect(),
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
                    && ((entry.field_path.is_empty() && !property.contains('.'))
                        || entry.field_path == property)
                    && index_applies_to_query(entry.direction, query_direction)
            })
        })
    }

    fn is_inline_struct_property_for(&self, label: &str, property_id: PropertyId) -> bool {
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES.with_borrow(|profiles| {
            RouterStore::new()
                .lookup_edge_label_id(self.graph_id, label)
                .ok()
                .and_then(|label_id| profiles.get_record(self.graph_id, label_id))
                .is_some_and(|record| {
                    record.inline_property_id() == Some(property_id) && record.is_inline_struct()
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

    /// Every Active vertex named index is order-preserving encoded and range-capable
    /// through `lookup_range` (vector indexes are a separate mechanism and never appear
    /// as `IndexedPropertyKind::Vertex` named-index rows), so range capability mirrors
    /// equality membership from the same catalog projection.
    fn is_vertex_property_range_indexed(&self, property: &str) -> bool {
        self.is_named_property_indexed(IndexedPropertyKind::Vertex, property)
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
            Some(label) => {
                let Some(property_id) = ROUTER_PROPERTY_CATALOG
                    .with_borrow(|catalog| catalog.get_id(self.graph_id, property))
                else {
                    return false;
                };
                if self.is_inline_struct_property_for(label, property_id) {
                    return false;
                }
                self.is_edge_indexed_for(label, property, direction)
            }
            None => {
                let Some(property_id) = ROUTER_PROPERTY_CATALOG
                    .with_borrow(|catalog| catalog.get_id(self.graph_id, property))
                else {
                    return false;
                };
                if ROUTER_EDGE_INLINE_PROPERTY_PROFILES.with_borrow(|profiles| {
                    profiles.has_inline_property(self.graph_id, property_id)
                }) {
                    return false;
                }
                self.is_edge_property_indexed(property)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::store::RouterStore;
    use crate::init::RouterInitArgs;
    use candid::Principal;
    use gleaph_graph_kernel::index::EdgeIndexDirection;

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
        let admin = Principal::from_slice(&[1; 29]);
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: None,
        });
        crate::facade::auth::grant_admins(&[admin]);
        crate::facade::store::catalog_test_support::register_graph(&store, admin, TEST_GRAPH);
        let graph_id = store.resolve_graph_id(TEST_GRAPH).expect("test graph");
        let property_id = crate::facade::store::catalog_test_support::intern_property(
            &store, admin, TEST_GRAPH, "region",
        );
        let stats = RouterGraphStats::from_property_ids(
            graph_id,
            [property_id].into_iter().collect(),
            BTreeSet::new(),
        );
        assert!(stats.is_vertex_property_indexed("region"));
        assert!(!stats.is_vertex_property_indexed("missing"));
    }

    #[test]
    fn adapter_resolves_range_capability_via_catalog() {
        let store = RouterStore::new();
        let admin = Principal::from_slice(&[1; 29]);
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: None,
        });
        crate::facade::auth::grant_admins(&[admin]);
        crate::facade::store::catalog_test_support::register_graph(&store, admin, TEST_GRAPH);
        let graph_id = store.resolve_graph_id(TEST_GRAPH).expect("test graph");
        let property_id = crate::facade::store::catalog_test_support::intern_property(
            &store, admin, TEST_GRAPH, "age",
        );
        let stats = RouterGraphStats::from_property_ids(
            graph_id,
            [property_id].into_iter().collect(),
            BTreeSet::new(),
        );
        assert!(stats.is_vertex_property_range_indexed("age"));
        assert!(!stats.is_vertex_property_range_indexed("missing"));
    }

    #[test]
    fn edge_index_subset_rule_uses_direction() {
        let store = RouterStore::new();
        let admin = Principal::from_slice(&[1; 29]);
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: None,
        });
        crate::facade::auth::grant_admins(&[admin]);
        crate::facade::store::catalog_test_support::register_graph(&store, admin, TEST_GRAPH);
        let graph_id = store.resolve_graph_id(TEST_GRAPH).expect("test graph");
        let _ = store
            .admin_intern_edge_label(admin, TEST_GRAPH, "KNOWS")
            .expect("label");
        let property_id = crate::facade::store::catalog_test_support::intern_property(
            &store, admin, TEST_GRAPH, "weight",
        );
        let stats = RouterGraphStats::from_catalog(
            graph_id,
            BTreeSet::new(),
            [property_id].into_iter().collect(),
            [EdgeIndexMembership {
                physical_index_id: PhysicalIndexId::new(1).expect("physical id"),
                catalog_epoch: 0,
                property_id,
                label_id: 1,
                direction: EdgeIndexDirection::Outgoing,
                field_path: String::new(),
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
    #[test]
    fn edge_property_index_for_none_fail_closed_when_any_label_is_inline() {
        use crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES;
        use crate::facade::stable::edge_inline_property_profiles::InlineScalarType;
        let store = RouterStore::new();
        let admin = Principal::from_slice(&[1; 29]);
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: None,
        });
        crate::facade::auth::grant_admins(&[admin]);
        crate::facade::store::catalog_test_support::register_graph(&store, admin, TEST_GRAPH);
        let graph_id = store.resolve_graph_id(TEST_GRAPH).expect("test graph");

        let road_label_id = store
            .admin_intern_edge_label(admin, TEST_GRAPH, "ROAD")
            .expect("intern ROAD");
        let _knows_label_id = store
            .admin_intern_edge_label(admin, TEST_GRAPH, "KNOWS")
            .expect("intern KNOWS");
        let property_id = crate::facade::store::catalog_test_support::intern_property(
            &store, admin, TEST_GRAPH, "distance",
        );

        // KNOWS has a sidecar edge index on distance; ROAD has the same property id as inline.
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Edge,
                label: "KNOWS".into(),
                property: "distance".into(),
                edge_direction: Some(gleaph_gql::types::EdgeDirection::AnyDirection),
            },
        ))
        .expect("create edge index on KNOWS.distance");

        ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow_mut(|s| {
                s.set_inline_scalar_schema(
                    graph_id,
                    road_label_id,
                    property_id,
                    InlineScalarType::U16,
                )
            })
            .expect("set inline schema on ROAD.distance");

        let stats = RouterGraphStats::from_catalog(
            graph_id,
            BTreeSet::new(),
            [property_id].into_iter().collect(),
            {
                use crate::facade::stable::indexed_catalog::load_graph_stats;
                load_graph_stats(graph_id).edge_indexes
            },
        );

        assert!(
            !stats.is_edge_property_indexed_for(
                None,
                "distance",
                gleaph_gql::types::EdgeDirection::AnyDirection,
            ),
            "wildcard label query must be fail-closed when any label has an inline slot for the property"
        );
        assert!(
            stats.is_edge_property_indexed_for(
                Some("KNOWS"),
                "distance",
                gleaph_gql::types::EdgeDirection::AnyDirection,
            ),
            "KNOWS.distance sidecar index must remain visible for a concrete label query"
        );
        assert!(
            !stats.is_edge_property_indexed_for(
                Some("ROAD"),
                "distance",
                gleaph_gql::types::EdgeDirection::AnyDirection,
            ),
            "ROAD.distance inline slot must not be reported as sidecar-indexed"
        );
    }
    #[test]
    fn edge_property_index_for_inline_struct_fail_closed_concrete_and_wildcard() {
        // Slice 24: an InlineStruct slot must not be reported as sidecar-indexed for either
        // concrete or wildcard label queries, even when a real edge index exists on another
        // label for the same property id.
        use crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES;
        use crate::facade::stable::edge_inline_property_profiles::{
            InlineScalarType, InlineStructLayout,
        };
        let store = RouterStore::new();
        let admin = Principal::from_slice(&[1; 29]);
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: None,
        });
        crate::facade::auth::grant_admins(&[admin]);
        crate::facade::store::catalog_test_support::register_graph(&store, admin, TEST_GRAPH);
        let graph_id = store.resolve_graph_id(TEST_GRAPH).expect("test graph");

        let affinity_label_id = store
            .admin_intern_edge_label(admin, TEST_GRAPH, "AFFINITY")
            .expect("intern AFFINITY");
        store
            .admin_intern_edge_label(admin, TEST_GRAPH, "KNOWS")
            .expect("intern KNOWS");
        let stats_property_id = crate::facade::store::catalog_test_support::intern_property(
            &store, admin, TEST_GRAPH, "stats",
        );

        // KNOWS has a sidecar edge index on stats; AFFINITY has the same property id as an
        // inline STRUCT slot.
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Edge,
                label: "KNOWS".into(),
                property: "stats".into(),
                edge_direction: Some(gleaph_gql::types::EdgeDirection::AnyDirection),
            },
        ))
        .expect("create edge index on KNOWS.stats");

        let layout = InlineStructLayout::from_fields(vec![
            ("score".into(), InlineScalarType::F32),
            ("confidence".into(), InlineScalarType::F32),
        ])
        .expect("seed layout");
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow_mut(|s| {
                s.set_inline_struct_schema(graph_id, affinity_label_id, stats_property_id, layout)
            })
            .expect("set inline struct schema on AFFINITY.stats");

        let stats = RouterGraphStats::from_catalog(
            graph_id,
            BTreeSet::new(),
            [stats_property_id].into_iter().collect(),
            {
                use crate::facade::stable::indexed_catalog::load_graph_stats;
                load_graph_stats(graph_id).edge_indexes
            },
        );

        assert!(
            !stats.is_edge_property_indexed_for(
                None,
                "stats",
                gleaph_gql::types::EdgeDirection::AnyDirection,
            ),
            "wildcard label query must be fail-closed when any label has an inline struct slot for the property"
        );
        assert!(
            stats.is_edge_property_indexed_for(
                Some("KNOWS"),
                "stats",
                gleaph_gql::types::EdgeDirection::AnyDirection,
            ),
            "KNOWS.stats sidecar index must remain visible for a concrete label query"
        );
        assert!(
            !stats.is_edge_property_indexed_for(
                Some("AFFINITY"),
                "stats",
                gleaph_gql::types::EdgeDirection::AnyDirection,
            ),
            "AFFINITY.stats inline struct slot must not be reported as sidecar-indexed"
        );
    }
}
