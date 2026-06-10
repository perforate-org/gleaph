//! Stable-memory-backed router fragments.

use std::cell::RefCell;

pub(crate) mod label_backfill;
pub(crate) mod label_telemetry;
pub(crate) mod memory;
pub(crate) mod placement_by_physical;

thread_local! {
    pub(crate) static ROUTER_CONTROLLERS: RefCell<memory::StableControllerSet> =
        RefCell::new(memory::init_controllers());

    pub(crate) static ROUTER_GRAPHS: RefCell<memory::StableGraphRegistry> =
        RefCell::new(memory::init_graphs());

    pub(crate) static ROUTER_SHARDS: RefCell<memory::StableShardRegistry> =
        RefCell::new(memory::init_shards());

    pub(crate) static ROUTER_SHARD_BY_GRAPH: RefCell<memory::StableShardByGraph> =
        RefCell::new(memory::init_shard_by_graph());

    pub(crate) static ROUTER_PLACEMENTS: RefCell<memory::StablePlacementMap> =
        RefCell::new(memory::init_placements());

    pub(crate) static ROUTER_PLACEMENT_BY_PHYSICAL: RefCell<memory::StablePlacementByPhysicalMap> =
        RefCell::new(memory::init_placement_by_physical());

    pub(crate) static ROUTER_LOGICAL_COUNTER: RefCell<memory::StableLogicalCounter> =
        RefCell::new(memory::init_logical_counter());

    pub(crate) static ROUTER_PENDING_LOGICAL: RefCell<memory::StablePendingLogical> =
        RefCell::new(memory::init_pending_logical());

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

    pub(crate) static ROUTER_APPLIED_LABEL_TELEMETRY: RefCell<memory::StableAppliedLabelTelemetrySet> =
        RefCell::new(memory::init_applied_label_telemetry());

    pub(crate) static ROUTER_MUTATION_BY_CLIENT_KEY: RefCell<memory::StableMutationByClientKey> =
        RefCell::new(memory::init_mutation_by_client_key());

    pub(crate) static ROUTER_PROPERTY_CATALOG: RefCell<memory::StablePropertyCatalog> =
        RefCell::new(memory::init_property_catalog());

    /// Per logical graph: which vertex/edge properties are indexed (planner catalog).
    pub(crate) static ROUTER_INDEXED_PROPERTIES: RefCell<
        std::collections::BTreeMap<String, crate::planner_stats::RouterGraphStats>,
    > = const { RefCell::new(std::collections::BTreeMap::new()) };

    pub(crate) static ROUTER_PREPARED_PLANS: RefCell<
        std::collections::BTreeMap<String, crate::prepared::PreparedPlanRecord>,
    > = const { RefCell::new(std::collections::BTreeMap::new()) };

    pub(crate) static ROUTER_LABEL_BACKFILL_STATE: RefCell<memory::StableLabelBackfillStateMap> =
        RefCell::new(memory::init_label_backfill_state());

    pub(crate) static ROUTER_AUTH_STATE: RefCell<memory::StableAuthState> =
        RefCell::new(memory::init_auth_state());
}
