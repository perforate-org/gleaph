//! Router canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry).
//!
//! MemoryIds are grouped by [`StableMemoryClass`] / inventory domain:
//! auth → registry → runtime config → idempotency → catalog → telemetry → maintenance.

use super::edge_inline_property_profiles::EdgeInlinePropertyProfileStore;
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

use gleaph_auth::{AuthState, GrantState};
use gleaph_gql_ic::graph_registry::GraphRegistryEntry;
use gleaph_graph_catalog::GraphCatalog;

use super::constraint_catalog::{ConstraintDefRecord, UniqueConstraintKey};
use super::indexed_catalog::{IndexDefRecord, NamedIndexKey};
use super::reservation_catalog::{ReservationRecord, UniqueReservationKey};
use super::vector_index_catalog::{VectorIndexDefRecord, VectorIndexKey};
use super::vector_maintenance_policy::VectorMaintenancePolicyRecord;
use crate::provisioning::config::ProvisionRuntimeConfig;
use crate::types::{
    IntentLockOwner, ProvisioningByGraphKey, ProvisioningIntentKey, ProvisioningRequestKey,
    RouterProvisioningRequest,
};
use candid::CandidType;
use ic_stable_memory_backend::{DefaultMemoryImpl, default_memory_impl};
use ic_stable_structures::memory_manager::MemoryId;
use ic_stable_structures::{BTreeMap, Cell};
use ic_stable_variable_memory_manager::{MemoryManager, VirtualMemory};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::{Deref, DerefMut};

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

// --- auth (canonical) ---
const ROUTER_AUTH_PRINCIPAL_RECORDS: MemoryId = MemoryId::new(0);
// --- auth: data-plane grant rows (ADR 0074 §6) ---
const ROUTER_AUTH_GRANT_ROWS: MemoryId = MemoryId::new(55);

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

// --- catalog: index planner + edge inline property bytes + graph type ---
const ROUTER_NAMED_INDEXES: MemoryId = MemoryId::new(19);
const ROUTER_NEXT_PHYSICAL_INDEX_ID: MemoryId = MemoryId::new(20);
const ROUTER_EDGE_INLINE_PROPERTY_PROFILES: MemoryId = MemoryId::new(21);
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

// --- durable bulk-load chunk receipts (ADR 0057) ---
pub(crate) const ROUTER_BULK_LOAD_CHUNK_RECEIPTS: MemoryId = MemoryId::new(49);

// --- schema migration ledger (ADR 0058) ---
pub(crate) const ROUTER_SCHEMA_MIGRATIONS: MemoryId = MemoryId::new(50);

// --- physical-index catalog epoch fence (ADR 0059) ---
pub(crate) const ROUTER_INDEX_CATALOG_EPOCH: MemoryId = MemoryId::new(51);

// --- catalog: opaque vector physical id allocation (ADR 0065) ---
const ROUTER_NEXT_VECTOR_INDEX_ID: MemoryId = MemoryId::new(52);

// --- direct vector-ingestion durable suffix ---
pub(crate) const ROUTER_VECTOR_INGEST_OUTBOX: MemoryId = MemoryId::new(53);

// --- retired physical posting namespaces pending a confirmed graph-index purge (ADR 0023 D6) ---
pub(crate) const ROUTER_INDEX_RETIRED: MemoryId = MemoryId::new(54);

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
pub(crate) type StableGrantState = GrantState<Memory>;

// --- registry ---
pub(crate) type StableGraphRegistry = BTreeMap<GraphId, GraphRegistryEntry, Memory>;
pub(crate) type StableShardRegistry = BTreeMap<GraphShardKey, RouterShardState, Memory>;
pub(crate) type StableShardByGraph = BTreeMap<Principal, GraphShardKey, Memory>;
pub(crate) type StableShardsByGraphId = BTreeMap<GraphId, GraphShardList, Memory>;
pub(crate) type StableGraphRuntimeConfigMap = BTreeMap<GraphId, GraphRuntimeConfig, Memory>;

/// Canonical Router-owned state for one registered shard.
///
/// [`ShardRegistryEntry`] is the public routing projection consumed across the Router boundary.
/// `vector_attach_epoch` is an internal durable fence: every new same-row Vector claim captures
/// it, unregister invalidates it, and only the exact epoch may publish Vector readiness after an
/// await. Keeping both in one MemoryId-2 value gives the Router one write owner without exposing
/// orchestration identity on the public shard-registry API.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct RouterShardState {
    pub(crate) entry: ShardRegistryEntry,
    pub(crate) vector_attach_epoch: u64,
}

impl RouterShardState {
    pub(crate) const fn new(entry: ShardRegistryEntry) -> Self {
        Self {
            entry,
            vector_attach_epoch: 0,
        }
    }
}

impl From<ShardRegistryEntry> for RouterShardState {
    fn from(entry: ShardRegistryEntry) -> Self {
        Self::new(entry)
    }
}

impl Deref for RouterShardState {
    type Target = ShardRegistryEntry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl DerefMut for RouterShardState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entry
    }
}

impl ic_stable_structures::Storable for RouterShardState {
    const BOUND: ic_stable_structures::storable::Bound =
        ic_stable_structures::storable::Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode RouterShardState"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode RouterShardState")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), RouterShardState).expect("decode RouterShardState")
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct GraphRuntimeConfig {
    pub element_id_encoding_key: [u8; 16],
    pub index_group_size: u32,
    pub index_cluster: Vec<Principal>,
    /// Next graph-local shard identity that has never been issued.
    pub next_shard_id: u64,
}

impl GraphRuntimeConfig {
    pub const fn with_element_id_encoding_key(key: ElementIdEncodingKey) -> Self {
        Self {
            element_id_encoding_key: key.0,
            index_group_size: 1,
            index_cluster: Vec::new(),
            next_shard_id: 0,
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
pub(crate) type StablePhysicalIndexIdAllocator = Cell<u64, Memory>;
pub(crate) type StableIndexCatalogEpoch = Cell<u64, Memory>;
pub(crate) type StableEdgeInlinePropertyProfileStore = EdgeInlinePropertyProfileStore<Memory>;
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
pub(crate) type StableVectorIndexIdAllocator = Cell<u32, Memory>;
pub(crate) type StableVectorMaintenancePolicyMap =
    BTreeMap<VectorIndexKey, VectorMaintenancePolicyRecord, Memory>;
pub(crate) type StableVectorIngestOutboxMap = BTreeMap<
    super::vector_ingest_outbox::VectorIngestOutboxKey,
    super::vector_ingest_outbox::VectorIngestOutboxValue,
    Memory,
>;
pub(crate) type StableIndexRetiredMap = BTreeMap<
    super::index_retirement::RetiredPhysicalIndexKey,
    super::index_retirement::RetiredIndexRecord,
    Memory,
>;

// --- provisioning (ADR 0035 Slice 1) ---
pub(crate) type StableProvisioningRequestMap =
    BTreeMap<ProvisioningRequestKey, RouterProvisioningRequest, Memory>;
pub(crate) type StableProvisioningByGraphMap =
    BTreeMap<ProvisioningByGraphKey, ProvisioningRequestKey, Memory>;
pub(crate) type StableProvisioningIntentLockMap =
    BTreeMap<ProvisioningIntentKey, IntentLockOwner, Memory>;

pub(crate) type StableProvisionConfig = Cell<ProvisionRuntimeConfig, Memory>;

pub(crate) type StableBulkLoadChunkReceiptMap = super::bulk_load::StableBulkLoadChunkReceiptMap;

pub(crate) type StableSchemaMigrationMap =
    BTreeMap<String, super::schema_migration::StableSchemaMigrationRecord, Memory>;

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

/// Initial Router policy: small catalogs/cells use little slack, while variable-sized and bursty
/// mutation domains receive larger extents. These values are persisted by the custom manager.
/// Stable-memory compatibility with the former upstream manager is intentionally not supported.
const ROUTER_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES: u16 = 2;
const ROUTER_MEMORY_MANAGER_POLICIES: &[(MemoryId, u16)] = &[
    (ROUTER_AUTH_PRINCIPAL_RECORDS, 2),
    (ROUTER_AUTH_GRANT_ROWS, 4),
    (ROUTER_GRAPHS, 4),
    (ROUTER_SHARDS, 4),
    (ROUTER_SHARD_BY_GRAPH, 2),
    (ROUTER_SHARDS_BY_GRAPH_ID, 2),
    (ROUTER_GRAPH_RUNTIME_CONFIG, 2),
    (ROUTER_MUTATION_COUNTER, 1),
    (ROUTER_MUTATION_BY_CLIENT_KEY, 16),
    (ROUTER_PREPARED_PLANS, 16),
    (ROUTER_VERTEX_LABEL_BY_NAME, 2),
    (ROUTER_VERTEX_LABEL_BY_ID, 2),
    (ROUTER_EDGE_LABEL_BY_NAME, 2),
    (ROUTER_EDGE_LABEL_BY_ID, 2),
    (ROUTER_PROPERTY_BY_NAME, 2),
    (ROUTER_PROPERTY_BY_ID, 2),
    (ROUTER_GRAPH_BY_NAME, 2),
    (ROUTER_GRAPH_BY_ID, 2),
    (ROUTER_INDEX_NAME_BY_NAME, 2),
    (ROUTER_INDEX_NAME_BY_ID, 2),
    (ROUTER_NAMED_INDEXES, 8),
    (ROUTER_NEXT_PHYSICAL_INDEX_ID, 1),
    (ROUTER_EDGE_INLINE_PROPERTY_PROFILES, 8),
    (ROUTER_GRAPH_TYPE_DEFINITIONS, 8),
    (ROUTER_GRAPH_SCHEMA_BINDINGS, 8),
    (ROUTER_GRAPH_TYPE_BY_NAME, 2),
    (ROUTER_GRAPH_TYPE_BY_ID, 2),
    (ROUTER_VERTEX_LABEL_STATS, 8),
    (ROUTER_EDGE_LABEL_STATS, 8),
    (ROUTER_VERTEX_LABEL_LIVE_BY_SHARD, 4),
    (ROUTER_EDGE_LABEL_LIVE_BY_SHARD, 4),
    (ROUTER_LABEL_STATS_PROJECTION, 2),
    (ROUTER_LABEL_BACKFILL_STATE, 2),
    (ROUTER_VERTEX_PROPERTY_BACKFILL_STATE, 2),
    (ROUTER_EDGE_BACKFILL_STATE, 2),
    (ROUTER_CONSTRAINT_NAME_BY_NAME, 2),
    (ROUTER_CONSTRAINT_NAME_BY_ID, 2),
    (ROUTER_UNIQUE_CONSTRAINTS, 8),
    (ROUTER_UNIQUE_RESERVATIONS, 16),
    (ROUTER_MUTATION_RESERVATION_INDEX, 4),
    (ROUTER_UNIQUE_EFFECT_PENDING, 8),
    (ROUTER_EMBEDDING_NAME_BY_NAME, 2),
    (ROUTER_EMBEDDING_NAME_BY_ID, 2),
    (ROUTER_VECTOR_INDEXES, 8),
    (ROUTER_VECTOR_DISPATCH_ACTIVATION, 1),
    (ROUTER_VECTOR_MAINTENANCE_POLICIES, 8),
    (ROUTER_PROVISIONING_REQUESTS, 16),
    (ROUTER_PROVISIONING_BY_GRAPH, 4),
    (ROUTER_PROVISIONING_INTENT_LOCK, 4),
    (ROUTER_PROVISION_CONFIG, 1),
    (ROUTER_BULK_LOAD_CHUNK_RECEIPTS, 16),
    (ROUTER_SCHEMA_MIGRATIONS, 16),
    (ROUTER_INDEX_CATALOG_EPOCH, 1),
    (ROUTER_NEXT_VECTOR_INDEX_ID, 1),
    (ROUTER_VECTOR_INGEST_OUTBOX, 16),
    (ROUTER_INDEX_RETIRED, 8),
];

thread_local! {
    pub(crate) static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init_with_policies(
            default_memory_impl(),
            ROUTER_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES,
            ROUTER_MEMORY_MANAGER_POLICIES,
        ));
}

// --- auth ---
pub(crate) fn init_auth_state() -> StableAuthState {
    AuthState::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_AUTH_PRINCIPAL_RECORDS)))
}

pub(crate) fn init_grant_state() -> StableGrantState {
    GrantState::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_AUTH_GRANT_ROWS)))
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

pub(crate) fn init_next_physical_index_id() -> StablePhysicalIndexIdAllocator {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_NEXT_PHYSICAL_INDEX_ID)),
        1,
    )
}

pub(crate) fn init_index_catalog_epoch() -> StableIndexCatalogEpoch {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_INDEX_CATALOG_EPOCH)),
        0,
    )
}

pub(crate) fn init_edge_inline_property_profiles() -> StableEdgeInlinePropertyProfileStore {
    EdgeInlinePropertyProfileStore::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_INLINE_PROPERTY_PROFILES)),
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

pub(crate) fn init_next_vector_index_id() -> StableVectorIndexIdAllocator {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_NEXT_VECTOR_INDEX_ID)),
        1,
    )
}

pub(crate) fn init_vector_ingest_outbox() -> StableVectorIngestOutboxMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VECTOR_INGEST_OUTBOX)))
}

pub(crate) fn init_index_retired() -> StableIndexRetiredMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_INDEX_RETIRED)))
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

pub(crate) fn init_provision_config() -> StableProvisionConfig {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROVISION_CONFIG)),
        ProvisionRuntimeConfig::default(),
    )
}

pub(crate) fn memory_manager_get_bulk_load_receipts() -> Memory {
    MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_BULK_LOAD_CHUNK_RECEIPTS))
}

pub(crate) fn init_bulk_load_chunk_receipts() -> StableBulkLoadChunkReceiptMap {
    super::bulk_load::init_bulk_load_chunk_receipts()
}

pub(crate) fn init_schema_migrations() -> StableSchemaMigrationMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_SCHEMA_MIGRATIONS)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
    use gleaph_graph_kernel::federation::{LocalVertexId, ShardId};
    use gleaph_graph_kernel::vector_index::{
        IndexedEmbeddingSpec, VectorEncoding, VectorIndexKind, VectorMetric,
    };
    use ic_stable_structures::VectorMemory;
    use std::collections::HashSet;

    #[test]
    fn initial_memory_policy_covers_each_router_region_once() {
        assert_eq!(ROUTER_MEMORY_MANAGER_POLICIES.len(), 56);
        let ids: HashSet<u8> = ROUTER_MEMORY_MANAGER_POLICIES
            .iter()
            .map(|(id, _)| {
                (0..=55)
                    .find(|candidate| *id == MemoryId::new(*candidate))
                    .expect("policy id is in the Router layout")
            })
            .collect();
        assert_eq!(ids.len(), 56);
        for id in 0..=55 {
            assert!(ids.contains(&id));
        }
    }

    #[test]
    fn graph_runtime_config_reopen_preserves_shard_high_water() {
        let memory = VectorMemory::default();
        let manager = ic_stable_structures::memory_manager::MemoryManager::init(memory.clone());
        let mut configs: BTreeMap<GraphId, GraphRuntimeConfig, _> =
            BTreeMap::init(manager.get(MemoryId::new(0)));
        let graph_id = GraphId::from_raw(7);
        let mut expected = GraphRuntimeConfig::with_element_id_encoding_key(
            ElementIdEncodingKey::host_test_fixture(),
        );
        expected.next_shard_id = u64::from(u32::MAX) + 1;
        configs.insert(graph_id, expected.clone());
        drop(configs);
        drop(manager);

        let reopened_manager = ic_stable_structures::memory_manager::MemoryManager::init(memory);
        let reopened: BTreeMap<GraphId, GraphRuntimeConfig, _> =
            BTreeMap::init(reopened_manager.get(MemoryId::new(0)));
        assert_eq!(reopened.get(&graph_id), Some(expected));
    }

    #[test]
    fn router_shard_state_reopen_preserves_vector_attach_epoch() {
        let memory = VectorMemory::default();
        let manager = ic_stable_structures::memory_manager::MemoryManager::init(memory.clone());
        let mut shards: BTreeMap<GraphShardKey, RouterShardState, _> =
            BTreeMap::init(manager.get(MemoryId::new(0)));
        let graph_id = GraphId::from_raw(7);
        let key = GraphShardKey::new(graph_id, ShardId::new(3));
        let expected = RouterShardState {
            entry: ShardRegistryEntry {
                shard_id: key.shard_id,
                graph_canister: Principal::from_slice(&[7; 29]),
                index_canister: Principal::from_slice(&[8; 29]),
                graph_id,
                registered_at_ns: 123,
                index_attached: true,
                vector_canister: Some(Principal::from_slice(&[9; 29])),
                vector_index_attached: false,
            },
            vector_attach_epoch: 42,
        };
        shards.insert(key, expected.clone());
        drop(shards);
        drop(manager);

        let reopened_manager = ic_stable_structures::memory_manager::MemoryManager::init(memory);
        let reopened: BTreeMap<GraphShardKey, RouterShardState, _> =
            BTreeMap::init(reopened_manager.get(MemoryId::new(0)));
        assert_eq!(reopened.get(&key), Some(expected));
    }

    /// Plan 0287 regression guard (closes the Plan 0286 P2 finding): the auth principal
    /// records (MemoryId 0) and the data-plane grant rows (MemoryId 55) must be wired to
    /// **distinct** memories. If both maps ever share one MemoryId, the second map's
    /// metadata overwrites the first's in place and this reopen test fails loudly instead
    /// of silently corrupting one of the collections.
    #[test]
    fn principal_records_and_grant_rows_are_independent_memories() {
        use gleaph_auth::{
            AdminCaps, Direction, GrantSubject, GraphOperation, GraphPrivilege, GraphResource,
            Privilege,
        };

        assert_ne!(
            ROUTER_AUTH_PRINCIPAL_RECORDS, ROUTER_AUTH_GRANT_ROWS,
            "auth collections must not share a MemoryId"
        );
        let memory = VectorMemory::default();
        let manager = ic_stable_structures::memory_manager::MemoryManager::init(memory.clone());
        let mut auth_state = AuthState::init(manager.get(ROUTER_AUTH_PRINCIPAL_RECORDS));
        let mut grants = GrantState::init(manager.get(ROUTER_AUTH_GRANT_ROWS));

        let principal = Principal::from_slice(&[0xC1; 29]);
        auth_state
            .upsert_caps(principal, AdminCaps::INDEX_CREATE)
            .expect("non-anonymous caps row");
        let privilege = Privilege::Graph(GraphPrivilege {
            graph: 7,
            operation: GraphOperation::Traverse(Some(Direction::Outgoing)),
            resource: GraphResource::EdgeLabel(3),
        });
        grants
            .grant(GrantSubject::Public, &privilege, None)
            .expect("public grant row");
        assert_eq!(auth_state.len(), 1);
        assert_eq!(grants.len(), 1);
        drop(grants);
        drop(auth_state);
        drop(manager);

        // Reopen through the production init helpers' memory ids and require each
        // collection to see exactly its own rows.
        let reopened_manager = ic_stable_structures::memory_manager::MemoryManager::init(memory);
        let reopened_auth = AuthState::init(reopened_manager.get(ROUTER_AUTH_PRINCIPAL_RECORDS));
        let reopened_grants = GrantState::init(reopened_manager.get(ROUTER_AUTH_GRANT_ROWS));
        assert_eq!(
            reopened_auth.caps_of(&principal),
            AdminCaps::INDEX_CREATE,
            "principal records must survive a reopen"
        );
        assert!(reopened_grants.contains(GrantSubject::Public, &privilege));
        assert_eq!(
            reopened_grants.len(),
            1,
            "the grants map must contain only grant rows"
        );
        assert_eq!(
            reopened_auth.len(),
            1,
            "the caps map must contain only principal rows"
        );
    }

    #[test]
    fn production_vector_regions_reopen_through_production_helpers() {
        let _guard = super::super::vector_ingest_outbox::test_lock();
        let mut allocator = init_next_vector_index_id();
        let mut outbox = init_vector_ingest_outbox();
        let previous_allocator = *allocator.get();
        let previous_outbox: Vec<_> = outbox
            .iter()
            .map(|entry| (*entry.key(), entry.value()))
            .collect();

        let mutation_id = 9_000_000_001;
        let state = super::super::vector_ingest_outbox::VectorIngestOutboxState {
            graph_id: GraphId::from_raw(1),
            graph_target: Principal::from_slice(&[8; 29]),
            vector_target: Principal::from_slice(&[9; 29]),
            shard_id: ShardId::new(2),
            local_vertex_id: LocalVertexId::from(42u32),
            spec: IndexedEmbeddingSpec {
                embedding_name_id: 3,
                index_id: 7,
                kind: VectorIndexKind::IvfFlat,
                encoding: VectorEncoding::F32,
                dims: 1,
                metric: VectorMetric::L2Squared,
                labels: vec![VertexLabelId::from_raw(1)],
            },
            mutation_id,
            bytes: vec![42, 0, 0, 0],
            phase: super::super::vector_ingest_outbox::VectorIngestIntentPhase::AwaitingVector,
        };
        let key = super::super::vector_ingest_outbox::VectorIngestOutboxKey::from_state(&state);
        let value = super::super::vector_ingest_outbox::VectorIngestOutboxValue::from_state(&state);
        outbox.clear_new();
        allocator.set(4_100_000_007);
        outbox.insert(key, value);
        drop(outbox);
        drop(allocator);

        let reopened_allocator = init_next_vector_index_id();
        let reopened_outbox = init_vector_ingest_outbox();
        assert_eq!(*reopened_allocator.get(), 4_100_000_007);
        let reopened_value = reopened_outbox.get(&key).expect("reopened outbox value");
        let reopened_state =
            super::super::vector_ingest_outbox::state_from_entry(key, reopened_value);
        assert_eq!(reopened_state, state);
        drop(reopened_outbox);
        drop(reopened_allocator);

        let mut restored_outbox = init_vector_ingest_outbox();
        restored_outbox.clear_new();
        for (key, value) in previous_outbox {
            restored_outbox.insert(key, value);
        }
        let mut restored_allocator = init_next_vector_index_id();
        restored_allocator.set(previous_allocator);
    }
}
