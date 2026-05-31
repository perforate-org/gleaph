//! Stable-memory-backed router fragments.

use std::cell::RefCell;

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

    pub(crate) static ROUTER_MIGRATION_COUNTER: RefCell<memory::StableMigrationCounter> =
        RefCell::new(memory::init_migration_counter());

    pub(crate) static ROUTER_LOGICAL_COUNTER: RefCell<memory::StableLogicalCounter> =
        RefCell::new(memory::init_logical_counter());

    pub(crate) static ROUTER_PENDING_LOGICAL: RefCell<memory::StablePendingLogical> =
        RefCell::new(memory::init_pending_logical());

    pub(crate) static ROUTER_VERTEX_LABEL_BY_NAME: RefCell<memory::StableLabelNameIntern> =
        RefCell::new(memory::init_vertex_label_by_name());

    pub(crate) static ROUTER_VERTEX_LABEL_BY_ID: RefCell<memory::StableLabelIdReverse> =
        RefCell::new(memory::init_vertex_label_by_id());

    pub(crate) static ROUTER_EDGE_LABEL_BY_NAME: RefCell<memory::StableLabelNameIntern> =
        RefCell::new(memory::init_edge_label_by_name());

    pub(crate) static ROUTER_EDGE_LABEL_BY_ID: RefCell<memory::StableLabelIdReverse> =
        RefCell::new(memory::init_edge_label_by_id());

    pub(crate) static ROUTER_PROPERTY_BY_NAME: RefCell<memory::StablePropertyNameIntern> =
        RefCell::new(memory::init_property_by_name());

    pub(crate) static ROUTER_PROPERTY_BY_ID: RefCell<memory::StablePropertyIdReverse> =
        RefCell::new(memory::init_property_by_id());

    /// Per logical graph: which vertex/edge properties are indexed (planner catalog).
    pub(crate) static ROUTER_INDEXED_PROPERTIES: RefCell<
        std::collections::BTreeMap<String, crate::planner_stats::RouterGraphStats>,
    > = const { RefCell::new(std::collections::BTreeMap::new()) };

    pub(crate) static ROUTER_PREPARED_PLANS: RefCell<
        std::collections::BTreeMap<String, crate::prepared::PreparedPlanRecord>,
    > = const { RefCell::new(std::collections::BTreeMap::new()) };

    pub(crate) static ROUTER_AUTH_STATE: RefCell<memory::StableAuthState> =
        RefCell::new(memory::init_auth_state());
}
