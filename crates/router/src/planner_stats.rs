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
use crate::facade::stable::{
    ROUTER_EDGE_LABEL_CATALOG, ROUTER_EDGE_PAYLOAD_PROFILES, ROUTER_PROPERTY_CATALOG,
};
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

    /// Export the per-graph indexed catalog in the wire form consumed by graph
    /// shards (ADR 0023 D1). Carries the same `(vertex, edge, edge-index)`
    /// membership the shard registry used to hold, sourced fresh per operation.
    pub(crate) fn to_indexed_property_catalog(
        &self,
    ) -> gleaph_graph_kernel::index::IndexedPropertyCatalog {
        gleaph_graph_kernel::index::IndexedPropertyCatalog {
            vertex_property_ids: self.vertex_property_ids.iter().map(|p| p.raw()).collect(),
            edge_property_ids: self.edge_property_ids.iter().map(|p| p.raw()).collect(),
            edge_indexes: self
                .edge_indexes
                .iter()
                .map(|m| gleaph_graph_kernel::index::IndexedEdgeMembership {
                    label_id: m.label_id,
                    property_id: m.property_id.raw(),
                    direction_tag: m.direction as u8,
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
            Some(label) => {
                // Defensive guard: a named inline schema (scalar or struct) is the canonical read source for its
                // (label, property) pair. Until inline-index maintenance exists, exclude the pair
                // from planner stats so a stale sidecar index cannot drive planning.
                let label_id = RouterStore::new()
                    .lookup_edge_label_id(self.graph_id, label)
                    .ok();
                let property_id = ROUTER_PROPERTY_CATALOG
                    .with_borrow(|catalog| catalog.get_id(self.graph_id, property));
                let inline_matches = label_id.is_some()
                    && property_id.is_some()
                    && ROUTER_EDGE_PAYLOAD_PROFILES.with_borrow(|store| {
                        store
                            .get_record(self.graph_id, label_id.unwrap())
                            .is_some_and(|record| {
                                record.is_named_inline()
                                    && record.inline_property_id() == property_id
                            })
                    });
                !inline_matches && self.is_edge_indexed_for(label, property, direction)
            }
            None => {
                // Fail-closed for wildcard / compound label expressions: if any edge label in this
                // graph has the property as its named inline slot, we cannot answer "is this property
                // indexed?" with a simple yes based only on a sidecar edge index for some other label.
                // The planner would otherwise fuse a predicate into an EdgeIndexScan that ignores
                // inline edges carrying the same property id in their payload.
                let Some(property_id) = ROUTER_PROPERTY_CATALOG
                    .with_borrow(|catalog| catalog.get_id(self.graph_id, property))
                else {
                    return false;
                };
                let any_label_is_inline = ROUTER_EDGE_LABEL_CATALOG.with_borrow(|catalog| {
                    catalog.iter_ids_for_graph(self.graph_id).any(|label_id| {
                        ROUTER_EDGE_PAYLOAD_PROFILES.with_borrow(|store| {
                            store
                                .get_record(self.graph_id, label_id)
                                .is_some_and(|record| {
                                    record.is_named_inline()
                                        && record.inline_property_id() == Some(property_id)
                                })
                        })
                    })
                });
                if any_label_is_inline {
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
        let admin = Principal::from_slice(&[1; 29]);
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: None,
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
    #[test]
    fn edge_property_index_for_none_fail_closed_when_any_label_is_inline() {
        use crate::facade::stable::ROUTER_EDGE_PAYLOAD_PROFILES;
        use crate::facade::stable::edge_inline_value_profiles::InlineScalarType;
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
        let property_id = store
            .admin_intern_property(admin, TEST_GRAPH, "distance")
            .expect("intern distance");

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

        ROUTER_EDGE_PAYLOAD_PROFILES
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
        use crate::facade::stable::ROUTER_EDGE_PAYLOAD_PROFILES;
        use crate::facade::stable::edge_inline_value_profiles::{
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
        let stats_property_id = store
            .admin_intern_property(admin, TEST_GRAPH, "stats")
            .expect("intern stats");

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
        ROUTER_EDGE_PAYLOAD_PROFILES
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
