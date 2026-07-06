//! Router canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry).
//!
//! MemoryIds are grouped by [`StableMemoryClass`] / inventory domain:
//! auth → registry → runtime config → idempotency → catalog → telemetry → maintenance.

use super::edge_payload_profiles::EdgePayloadProfileStore;
use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::bidirectional_catalog::DenseIndexNamePolicy;
use gleaph_graph_kernel::bidirectional_catalog::{
    BidirectionalCatalog, DenseConstraintNamePolicy, DenseEdgeLabelPolicy,
    DenseEmbeddingNamePolicy, DenseMaxPlusOnePolicy, SparseFromOnePolicy,
};
use gleaph_graph_kernel::entry::{
    ConstraintNameId, EdgeLabelId, EmbeddingNameId, GraphId, GraphTypeId, IndexNameId, PropertyId,
    VertexLabelId,
};
use gleaph_graph_kernel::federation::{
    BackfillShardState, EdgeBackfillShardState, ElementIdEncodingKey, GraphShardKey, ShardId,
    ShardRegistryEntry,
};
use gleaph_graph_kernel::scoped_name_catalog::GraphScopedNameCatalog;

use gleaph_auth::AuthState;
use gleaph_gql_ic::graph_registry::GraphRegistryEntry;
use gleaph_graph_catalog::GraphCatalog;

use super::constraint_catalog::{ConstraintDefRecord, UniqueConstraintKey};
use super::indexed_catalog::{IndexDefRecord, IndexedPropertyKey, NamedIndexKey};
use super::reservation_catalog::{ReservationRecord, UniqueReservationKey};
use super::vector_index_catalog::{VectorIndexDefRecord, VectorIndexKey};
use super::vector_maintenance_policy::VectorMaintenancePolicyRecord;
use crate::types::{
    IntentLockMarker, ProvisioningByGraphKey, ProvisioningIntentKey, ProvisioningRequestKey,
    RouterProvisioningRequest,
};
use candid::CandidType;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{BTreeMap, BTreeSet, Cell, DefaultMemoryImpl};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

// --- auth (canonical) ---
const ROUTER_AUTH_PRINCIPAL_RECORDS: MemoryId = MemoryId::new(0);

// --- registry (canonical) ---
const ROUTER_GRAPHS: MemoryId = MemoryId::new(1);
const ROUTER_SHARDS: MemoryId = MemoryId::new(2);
const ROUTER_SHARD_BY_GRAPH: MemoryId = MemoryId::new(3);
const ROUTER_SHARDS_BY_GRAPH_ID: MemoryId = MemoryId::new(4);

// --- runtime config (canonical, per GraphId) ---
const ROUTER_GRAPH_RUNTIME_CONFIG: MemoryId = MemoryId::new(5);

// --- idempotency / prepared queries (canonical) ---
const ROUTER_MUTATION_COUNTER: MemoryId = MemoryId::new(6);
const ROUTER_MUTATION_BY_CLIENT_KEY: MemoryId = MemoryId::new(7);
const ROUTER_PREPARED_PLANS: MemoryId = MemoryId::new(8);

// --- catalog: label / property / graph / index resolution ---
const ROUTER_VERTEX_LABEL_BY_NAME: MemoryId = MemoryId::new(9);
const ROUTER_VERTEX_LABEL_BY_ID: MemoryId = MemoryId::new(10);
const ROUTER_EDGE_LABEL_BY_NAME: MemoryId = MemoryId::new(11);
const ROUTER_EDGE_LABEL_BY_ID: MemoryId = MemoryId::new(12);
const ROUTER_PROPERTY_BY_NAME: MemoryId = MemoryId::new(13);
const ROUTER_PROPERTY_BY_ID: MemoryId = MemoryId::new(14);
const ROUTER_GRAPH_BY_NAME: MemoryId = MemoryId::new(15);
const ROUTER_GRAPH_BY_ID: MemoryId = MemoryId::new(16);
const ROUTER_INDEX_NAME_BY_NAME: MemoryId = MemoryId::new(17);
const ROUTER_INDEX_NAME_BY_ID: MemoryId = MemoryId::new(18);

// --- catalog: index planner + edge payload + graph type ---
const ROUTER_NAMED_INDEXES: MemoryId = MemoryId::new(19);
const ROUTER_INDEXED_PROPERTY_SET: MemoryId = MemoryId::new(20);
const ROUTER_EDGE_PAYLOAD_PROFILES: MemoryId = MemoryId::new(21);
const ROUTER_GRAPH_TYPE_DEFINITIONS: MemoryId = MemoryId::new(22);
const ROUTER_GRAPH_SCHEMA_BINDINGS: MemoryId = MemoryId::new(23);
const ROUTER_GRAPH_TYPE_BY_NAME: MemoryId = MemoryId::new(24);
const ROUTER_GRAPH_TYPE_BY_ID: MemoryId = MemoryId::new(25);

// --- telemetry ---
const ROUTER_VERTEX_LABEL_STATS: MemoryId = MemoryId::new(26);
const ROUTER_EDGE_LABEL_STATS: MemoryId = MemoryId::new(27);
const ROUTER_VERTEX_LABEL_LIVE_BY_SHARD: MemoryId = MemoryId::new(28);
const ROUTER_EDGE_LABEL_LIVE_BY_SHARD: MemoryId = MemoryId::new(29);
const ROUTER_LABEL_STATS_PROJECTION: MemoryId = MemoryId::new(30);

// --- maintenance (backfill cursors) ---
const ROUTER_LABEL_BACKFILL_STATE: MemoryId = MemoryId::new(31);
const ROUTER_VERTEX_PROPERTY_BACKFILL_STATE: MemoryId = MemoryId::new(32);
const ROUTER_EDGE_BACKFILL_STATE: MemoryId = MemoryId::new(33);

// --- catalog: cross-shard uniqueness constraints (ADR 0030) ---
const ROUTER_CONSTRAINT_NAME_BY_NAME: MemoryId = MemoryId::new(34);
const ROUTER_CONSTRAINT_NAME_BY_ID: MemoryId = MemoryId::new(35);
const ROUTER_UNIQUE_CONSTRAINTS: MemoryId = MemoryId::new(36);
const ROUTER_UNIQUE_RESERVATIONS: MemoryId = MemoryId::new(37);
const ROUTER_MUTATION_RESERVATION_INDEX: MemoryId = MemoryId::new(38);
const ROUTER_UNIQUE_EFFECT_PENDING: MemoryId = MemoryId::new(39);

// --- catalog: derived vector index (ADR 0031) ---
const ROUTER_EMBEDDING_NAME_BY_NAME: MemoryId = MemoryId::new(40);
const ROUTER_EMBEDDING_NAME_BY_ID: MemoryId = MemoryId::new(41);
const ROUTER_VECTOR_INDEXES: MemoryId = MemoryId::new(42);

// --- control: global vector dispatch activation flag (ADR 0031 Slice 4) ---
const ROUTER_VECTOR_DISPATCH_ACTIVATION: MemoryId = MemoryId::new(43);

// --- policy: per-(graph, index) vector maintenance policy (ADR 0031 Slice 10) ---
const ROUTER_VECTOR_MAINTENANCE_POLICIES: MemoryId = MemoryId::new(44);

// --- provisioning: pre-creation issuance intent catalog (ADR 0035 Slice 1) ---
const ROUTER_PROVISIONING_REQUESTS: MemoryId = MemoryId::new(45);
const ROUTER_PROVISIONING_BY_GRAPH: MemoryId = MemoryId::new(46);
const ROUTER_PROVISIONING_INTENT_LOCK: MemoryId = MemoryId::new(47);

// --- provisioning runtime config (ADR 0035 Slice 5) ---
const ROUTER_PROVISION_CONFIG: MemoryId = MemoryId::new(48);

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GraphShardList {
    pub shard_ids: Vec<ShardId>,
}

impl ic_stable_structures::Storable for GraphShardList {
    const BOUND: ic_stable_structures::storable::Bound =
        ic_stable_structures::storable::Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = Vec::with_capacity(4 + self.shard_ids.len() * 4);
        out.extend_from_slice(&(self.shard_ids.len() as u32).to_le_bytes());
        for shard_id in &self.shard_ids {
            out.extend_from_slice(&shard_id.to_le_bytes());
        }
        Cow::Owned(out)
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.shard_ids.len() * 4);
        out.extend_from_slice(&(self.shard_ids.len() as u32).to_le_bytes());
        for shard_id in self.shard_ids {
            out.extend_from_slice(&shard_id.to_le_bytes());
        }
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let len = u32::from_le_bytes(bytes[0..4].try_into().expect("shard list length")) as usize;
        let mut shard_ids = Vec::with_capacity(len);
        for i in 0..len {
            let start = 4 + i * 4;
            let raw = bytes[start..start + 4].try_into().expect("shard id bytes");
            shard_ids.push(ShardId::from_le_bytes(raw));
        }
        Self { shard_ids }
    }
}

// --- auth ---
pub(crate) type StableAuthState = AuthState<Memory>;

// --- registry ---
pub(crate) type StableGraphRegistry = BTreeMap<GraphId, GraphRegistryEntry, Memory>;
pub(crate) type StableShardRegistry = BTreeMap<GraphShardKey, ShardRegistryEntry, Memory>;
pub(crate) type StableShardByGraph = BTreeMap<Principal, GraphShardKey, Memory>;
pub(crate) type StableShardsByGraphId = BTreeMap<GraphId, GraphShardList, Memory>;
pub(crate) type StableGraphRuntimeConfigMap = BTreeMap<GraphId, GraphRuntimeConfig, Memory>;

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct GraphRuntimeConfig {
    pub element_id_encoding_key: [u8; 16],
    pub index_group_size: u32,
    pub index_cluster: Vec<Principal>,
}

impl GraphRuntimeConfig {
    pub const fn with_element_id_encoding_key(key: ElementIdEncodingKey) -> Self {
        Self {
            element_id_encoding_key: key.0,
            index_group_size: 1,
            index_cluster: Vec::new(),
        }
    }
}

impl ic_stable_structures::Storable for GraphRuntimeConfig {
    const BOUND: ic_stable_structures::storable::Bound =
        ic_stable_structures::storable::Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode GraphRuntimeConfig"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode GraphRuntimeConfig")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), GraphRuntimeConfig).expect("decode GraphRuntimeConfig")
    }
}

// --- idempotency / prepared queries ---
pub(crate) type StableMutationCounter = Cell<u64, Memory>;
pub(crate) type StableMutationByClientKey = BTreeMap<
    super::label_stats::ClientMutationKey,
    super::label_stats::RouterMutationRecord,
    Memory,
>;
pub(crate) type StablePreparedPlanMap = BTreeMap<
    super::prepared_catalog::PreparedPlanKey,
    super::prepared_catalog::PreparedPlanRecord,
    Memory,
>;

// --- catalog ---
pub(crate) type StableVertexLabelCatalog =
    GraphScopedNameCatalog<VertexLabelId, Memory, Memory, DenseMaxPlusOnePolicy>;
pub(crate) type StableEdgeLabelCatalog =
    GraphScopedNameCatalog<EdgeLabelId, Memory, Memory, DenseEdgeLabelPolicy>;
pub(crate) type StablePropertyCatalog =
    GraphScopedNameCatalog<PropertyId, Memory, Memory, DenseMaxPlusOnePolicy>;
pub(crate) type StableGraphCatalog =
    BidirectionalCatalog<GraphId, Memory, Memory, DenseMaxPlusOnePolicy>;
pub(crate) type StableIndexNameCatalog =
    GraphScopedNameCatalog<IndexNameId, Memory, Memory, DenseIndexNamePolicy>;
pub(crate) type StableNamedIndexMap = BTreeMap<NamedIndexKey, IndexDefRecord, Memory>;
pub(crate) type StableIndexedPropertySet = BTreeSet<IndexedPropertyKey, Memory>;
pub(crate) type StableEdgePayloadProfileStore = EdgePayloadProfileStore<Memory>;
pub(crate) type StableGqlGraphCatalog = GraphCatalog<Memory, Memory>;
pub(crate) type StableGraphTypeNameCatalog =
    BidirectionalCatalog<GraphTypeId, Memory, Memory, SparseFromOnePolicy>;
pub(crate) type StableConstraintNameCatalog =
    GraphScopedNameCatalog<ConstraintNameId, Memory, Memory, DenseConstraintNamePolicy>;
pub(crate) type StableUniqueConstraintMap =
    BTreeMap<UniqueConstraintKey, ConstraintDefRecord, Memory>;
pub(crate) type StableUniqueReservationMap =
    BTreeMap<UniqueReservationKey, ReservationRecord, Memory>;
pub(crate) type StableMutationReservationIndex = BTreeMap<
    gleaph_graph_kernel::plan_exec::MutationId,
    super::label_stats::MutationReservationIndexEntry,
    Memory,
>;
pub(crate) type StableUniqueEffectPendingMap = BTreeMap<
    super::unique_effect_pending::UniqueEffectPendingKey,
    super::unique_effect_pending::PendingEffectRecord,
    Memory,
>;
pub(crate) type StableEmbeddingNameCatalog =
    GraphScopedNameCatalog<EmbeddingNameId, Memory, Memory, DenseEmbeddingNamePolicy>;
pub(crate) type StableVectorIndexMap = BTreeMap<VectorIndexKey, VectorIndexDefRecord, Memory>;
pub(crate) type StableVectorMaintenancePolicyMap =
    BTreeMap<VectorIndexKey, VectorMaintenancePolicyRecord, Memory>;

// --- provisioning (ADR 0035 Slice 1) ---
pub(crate) type StableProvisioningRequestMap =
    BTreeMap<ProvisioningRequestKey, RouterProvisioningRequest, Memory>;
pub(crate) type StableProvisioningByGraphMap =
    BTreeMap<ProvisioningByGraphKey, ProvisioningRequestKey, Memory>;
pub(crate) type StableProvisioningIntentLockMap =
    BTreeMap<ProvisioningIntentKey, IntentLockMarker, Memory>;

pub(crate) type StableProvisionConfigMap =
    BTreeMap<(), crate::provisioning::config::ProvisionRuntimeConfig, Memory>;

// --- telemetry ---
pub(crate) type StableLabelStatsMap =
    BTreeMap<super::label_stats::GraphLabelKey, super::label_stats::LabelStats, Memory>;
pub(crate) type StableLabelShardLiveMap =
    BTreeMap<super::label_stats::GraphLabelShardKey, u64, Memory>;
pub(crate) type StableLabelStatsProjectionMap = BTreeMap<GraphShardKey, u64, Memory>;

// --- maintenance ---
pub(crate) type StableLabelBackfillStateMap = BTreeMap<GraphShardKey, BackfillShardState, Memory>;
pub(crate) type StableVertexPropertyBackfillStateMap =
    BTreeMap<GraphShardKey, BackfillShardState, Memory>;
pub(crate) type StableEdgeBackfillStateMap =
    BTreeMap<GraphShardKey, EdgeBackfillShardState, Memory>;

thread_local! {
    pub(crate) static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

// --- auth ---
pub(crate) fn init_auth_state() -> StableAuthState {
    AuthState::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_AUTH_PRINCIPAL_RECORDS)))
}

// --- registry ---
pub(crate) fn init_graphs() -> StableGraphRegistry {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_GRAPHS)))
}

pub(crate) fn init_shards() -> StableShardRegistry {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_SHARDS)))
}

pub(crate) fn init_shard_by_graph() -> StableShardByGraph {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_SHARD_BY_GRAPH)))
}

pub(crate) fn init_shards_by_graph_id() -> StableShardsByGraphId {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_SHARDS_BY_GRAPH_ID)))
}

pub(crate) fn init_graph_runtime_config() -> StableGraphRuntimeConfigMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_GRAPH_RUNTIME_CONFIG)))
}

// --- idempotency / prepared queries ---
pub(crate) fn init_mutation_counter() -> StableMutationCounter {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_MUTATION_COUNTER)),
        0u64,
    )
}

pub(crate) fn init_mutation_by_client_key() -> StableMutationByClientKey {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_MUTATION_BY_CLIENT_KEY)))
}

pub(crate) fn init_prepared_plans() -> StablePreparedPlanMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PREPARED_PLANS)))
}

// --- catalog ---
pub(crate) fn init_vertex_label_catalog() -> StableVertexLabelCatalog {
    GraphScopedNameCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_BY_ID)),
    )
}

pub(crate) fn init_edge_label_catalog() -> StableEdgeLabelCatalog {
    GraphScopedNameCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_BY_ID)),
    )
}

pub(crate) fn init_property_catalog() -> StablePropertyCatalog {
    GraphScopedNameCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROPERTY_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROPERTY_BY_ID)),
    )
}

pub(crate) fn init_graph_catalog() -> StableGraphCatalog {
    BidirectionalCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_GRAPH_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_GRAPH_BY_ID)),
    )
}

pub(crate) fn init_index_name_catalog() -> StableIndexNameCatalog {
    GraphScopedNameCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_INDEX_NAME_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_INDEX_NAME_BY_ID)),
    )
}

pub(crate) fn init_named_indexes() -> StableNamedIndexMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_NAMED_INDEXES)))
}

pub(crate) fn init_indexed_property_set() -> StableIndexedPropertySet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_INDEXED_PROPERTY_SET)))
}

pub(crate) fn init_edge_payload_profiles() -> StableEdgePayloadProfileStore {
    EdgePayloadProfileStore::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_PAYLOAD_PROFILES)),
    )
}

pub(crate) fn init_gql_graph_catalog() -> StableGqlGraphCatalog {
    GraphCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_GRAPH_TYPE_DEFINITIONS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_GRAPH_SCHEMA_BINDINGS)),
    )
}

pub(crate) fn init_graph_type_name_catalog() -> StableGraphTypeNameCatalog {
    BidirectionalCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_GRAPH_TYPE_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_GRAPH_TYPE_BY_ID)),
    )
}

pub(crate) fn init_constraint_name_catalog() -> StableConstraintNameCatalog {
    GraphScopedNameCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_CONSTRAINT_NAME_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_CONSTRAINT_NAME_BY_ID)),
    )
}

pub(crate) fn init_unique_constraints() -> StableUniqueConstraintMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_UNIQUE_CONSTRAINTS)))
}

pub(crate) fn init_unique_reservations() -> StableUniqueReservationMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_UNIQUE_RESERVATIONS)))
}

pub(crate) fn init_mutation_reservation_index() -> StableMutationReservationIndex {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_MUTATION_RESERVATION_INDEX)))
}

pub(crate) fn init_unique_effect_pending() -> StableUniqueEffectPendingMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_UNIQUE_EFFECT_PENDING)))
}

pub(crate) fn init_embedding_name_catalog() -> StableEmbeddingNameCatalog {
    GraphScopedNameCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EMBEDDING_NAME_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EMBEDDING_NAME_BY_ID)),
    )
}

pub(crate) fn init_vector_indexes() -> StableVectorIndexMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VECTOR_INDEXES)))
}

pub(crate) fn init_vector_maintenance_policies() -> StableVectorMaintenancePolicyMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VECTOR_MAINTENANCE_POLICIES)))
}

// --- provisioning (ADR 0035 Slice 1) ---
pub(crate) fn init_provisioning_requests() -> StableProvisioningRequestMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROVISIONING_REQUESTS)))
}

pub(crate) fn init_provisioning_by_graph() -> StableProvisioningByGraphMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROVISIONING_BY_GRAPH)))
}

pub(crate) fn init_provisioning_intent_locks() -> StableProvisioningIntentLockMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROVISIONING_INTENT_LOCK)))
}

pub(crate) fn init_provision_config() -> StableProvisionConfigMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROVISION_CONFIG)))
}

// --- control ---
/// Global derived-vector-dispatch activation flag (ADR 0031 Slice 4). `false` (default, off) keeps
/// production dispatch/backfill fail-closed; an RBAC-gated admin endpoint flips it. Reversible.
pub(crate) type StableVectorDispatchActivation = Cell<bool, Memory>;

pub(crate) fn init_vector_dispatch_activation() -> StableVectorDispatchActivation {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VECTOR_DISPATCH_ACTIVATION)),
        false,
    )
}

// --- telemetry ---
pub(crate) fn init_vertex_label_stats() -> StableLabelStatsMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_STATS)))
}

pub(crate) fn init_edge_label_stats() -> StableLabelStatsMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_STATS)))
}

pub(crate) fn init_vertex_label_live_by_shard() -> StableLabelShardLiveMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_LIVE_BY_SHARD)))
}

pub(crate) fn init_edge_label_live_by_shard() -> StableLabelShardLiveMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_LIVE_BY_SHARD)))
}

pub(crate) fn init_label_stats_projection() -> StableLabelStatsProjectionMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_LABEL_STATS_PROJECTION)))
}

// --- maintenance ---
pub(crate) fn init_label_backfill_state() -> StableLabelBackfillStateMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_LABEL_BACKFILL_STATE)))
}

pub(crate) fn init_vertex_property_backfill_state() -> StableVertexPropertyBackfillStateMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_PROPERTY_BACKFILL_STATE)))
}

pub(crate) fn init_edge_backfill_state() -> StableEdgeBackfillStateMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_BACKFILL_STATE)))
}
