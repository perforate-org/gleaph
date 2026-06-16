//! Stable-memory-backed router fragments.

use std::cell::RefCell;

pub(crate) mod edge_payload_profiles;
pub(crate) mod graph_catalog;
pub(crate) mod graph_type_catalog;
pub(crate) mod graph_type_name_catalog;
pub(crate) mod index_name_catalog;
pub(crate) mod indexed_catalog;
pub(crate) mod label_stats;
pub(crate) mod layout;
pub(crate) mod memory;
pub(crate) mod prepared_catalog;
pub(crate) mod scoped_name_catalog;

thread_local! {
    pub(crate) static ROUTER_CONTROLLERS: RefCell<memory::StableControllerSet> =
        RefCell::new(memory::init_controllers());

    pub(crate) static ROUTER_GRAPHS: RefCell<memory::StableGraphRegistry> =
        RefCell::new(memory::init_graphs());

    pub(crate) static ROUTER_GRAPH_CATALOG: RefCell<memory::StableGraphCatalog> =
        RefCell::new(memory::init_graph_catalog());

    pub(crate) static ROUTER_INDEX_NAME_CATALOG: RefCell<memory::StableIndexNameCatalog> =
        RefCell::new(memory::init_index_name_catalog());

    pub(crate) static ROUTER_SHARDS_BY_GRAPH_ID: RefCell<memory::StableShardsByGraphId> =
        RefCell::new(memory::init_shards_by_graph_id());

    pub(crate) static ROUTER_SHARDS: RefCell<memory::StableShardRegistry> =
        RefCell::new(memory::init_shards());

    pub(crate) static ROUTER_SHARD_BY_GRAPH: RefCell<memory::StableShardByGraph> =
        RefCell::new(memory::init_shard_by_graph());

    pub(crate) static ROUTER_PLACEMENTS: RefCell<memory::StablePlacementMap> =
        RefCell::new(memory::init_placements());

    pub(crate) static ROUTER_VERTEX_LABEL_CATALOG: RefCell<memory::StableVertexLabelCatalog> =
        RefCell::new(memory::init_vertex_label_catalog());

    pub(crate) static ROUTER_EDGE_LABEL_CATALOG: RefCell<memory::StableEdgeLabelCatalog> =
        RefCell::new(memory::init_edge_label_catalog());

    pub(crate) static ROUTER_VERTEX_LABEL_STATS: RefCell<memory::StableLabelStatsMap> =
        RefCell::new(memory::init_vertex_label_stats());

    pub(crate) static ROUTER_EDGE_LABEL_STATS: RefCell<memory::StableLabelStatsMap> =
        RefCell::new(memory::init_edge_label_stats());

    pub(crate) static ROUTER_VERTEX_LABEL_LIVE_BY_SHARD: RefCell<memory::StableLabelShardLiveMap> =
        RefCell::new(memory::init_vertex_label_live_by_shard());

    pub(crate) static ROUTER_EDGE_LABEL_LIVE_BY_SHARD: RefCell<memory::StableLabelShardLiveMap> =
        RefCell::new(memory::init_edge_label_live_by_shard());

    pub(crate) static ROUTER_MUTATION_COUNTER: RefCell<memory::StableMutationCounter> =
        RefCell::new(memory::init_mutation_counter());

    pub(crate) static ROUTER_LABEL_STATS_PROJECTION: RefCell<memory::StableLabelStatsProjectionMap> =
        RefCell::new(memory::init_label_stats_projection());

    pub(crate) static ROUTER_MUTATION_BY_CLIENT_KEY: RefCell<memory::StableMutationByClientKey> =
        RefCell::new(memory::init_mutation_by_client_key());

    pub(crate) static ROUTER_PROPERTY_CATALOG: RefCell<memory::StablePropertyCatalog> =
        RefCell::new(memory::init_property_catalog());

    /// `(graph, index_name) → index definition` (ADR 0009 DDL metadata).
    pub(crate) static ROUTER_NAMED_INDEXES: RefCell<memory::StableNamedIndexMap> =
        RefCell::new(memory::init_named_indexes());

    /// `(graph, kind, property_id)` membership for planner + shard registry fan-out.
    pub(crate) static ROUTER_INDEXED_PROPERTY_SET: RefCell<memory::StableIndexedPropertySet> =
        RefCell::new(memory::init_indexed_property_set());

    pub(crate) static ROUTER_PREPARED_PLANS: RefCell<memory::StablePreparedPlanMap> =
        RefCell::new(memory::init_prepared_plans());

    pub(crate) static ROUTER_GQL_GRAPH_CATALOG: RefCell<memory::StableGqlGraphCatalog> =
        RefCell::new(memory::init_gql_graph_catalog());

    pub(crate) static ROUTER_GRAPH_TYPE_CATALOG: RefCell<memory::StableGraphTypeNameCatalog> =
        RefCell::new(memory::init_graph_type_name_catalog());

    pub(crate) static ROUTER_LABEL_BACKFILL_STATE: RefCell<memory::StableLabelBackfillStateMap> =
        RefCell::new(memory::init_label_backfill_state());

    pub(crate) static ROUTER_PROPERTY_BACKFILL_STATE: RefCell<memory::StablePropertyBackfillStateMap> =
        RefCell::new(memory::init_property_backfill_state());

    pub(crate) static ROUTER_EDGE_BACKFILL_STATE: RefCell<memory::StableEdgeBackfillStateMap> =
        RefCell::new(memory::init_edge_backfill_state());

    pub(crate) static ROUTER_AUTH_STATE: RefCell<memory::StableAuthState> =
        RefCell::new(memory::init_auth_state());

    pub(crate) static ROUTER_EDGE_PAYLOAD_PROFILES: RefCell<memory::StableEdgePayloadProfileStore> =
        RefCell::new(memory::init_edge_payload_profiles());
}
