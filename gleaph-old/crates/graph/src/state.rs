use std::cell::RefCell;

use candid::{decode_one, encode_one};
use gleaph_pma::abp_tree::{ABP_PAGE_SIZE, ABP_STORE_HEADER_LEN, AbpStoreHeader};
use gleaph_pma::{
    AbpPropertyStore, AbpSecondaryEqIndex, GraphOverlaySnapshot, Memory,
    MemoryError, PmaGraph, VertexMetaTable, VertexTombstoneBitset, layout,
    region_manager::{
        GRAPH_META_RESERVED_LEN, GRAPH_REGIONS_META_LEN, GRAPH_REGIONS_META_OFFSET,
        ReservedPersistMeta, ReservedRegionsMeta,
        refresh_reserved_abp_region_lengths_from_headers as refresh_abp_region_lengths_from_headers,
    },
};
// Type aliases for backward compat within this module.
type GraphPersistMeta = ReservedPersistMeta;
type GraphRegionsMeta = ReservedRegionsMeta;
use candid::Principal;
use gleaph_types::{
    AccessLevel, AclEntry, EntityType, GleaphError, IndexType, OperationalMetrics, UsageQuota,
};
use serde::{Deserialize, Serialize};

thread_local! {
    pub static STATE: RefCell<Option<PmaGraph<IcStableMemory>>> = const { RefCell::new(None) };
    static CONFIG: RefCell<GraphCanisterConfig> = const { RefCell::new(GraphCanisterConfig { initial_vertex_capacity: 1024 }) };
    static LAST_PERSIST_META: RefCell<Option<GraphPersistMeta>> = const { RefCell::new(None) };
    static METRICS: RefCell<OperationalMetrics> = const { RefCell::new(OperationalMetrics {
        query_count: 0,
        mutation_count: 0,
        rejected_count: 0,
        algorithm_calls: 0,
        stable_memory_bytes: 0,
    }) };
    static QUOTA: RefCell<UsageQuota> = const { RefCell::new(UsageQuota {
        max_vertices: 0,
        max_edges: 0,
    }) };
}

pub fn with_metrics<F, R>(f: F) -> R
where
    F: FnOnce(&OperationalMetrics) -> R,
{
    METRICS.with(|m| f(&m.borrow()))
}

pub fn increment_query_count() {
    METRICS.with(|m| m.borrow_mut().query_count += 1);
}

pub fn increment_mutation_count() {
    METRICS.with(|m| m.borrow_mut().mutation_count += 1);
}

pub fn increment_rejected_count() {
    METRICS.with(|m| m.borrow_mut().rejected_count += 1);
}

pub fn increment_algorithm_calls() {
    METRICS.with(|m| m.borrow_mut().algorithm_calls += 1);
}

pub fn get_quota() -> UsageQuota {
    QUOTA.with(|q| q.borrow().clone())
}

pub fn set_quota(quota: UsageQuota) {
    QUOTA.with(|q| *q.borrow_mut() = quota);
}

thread_local! {
    /// ACL overrides keyed by principal. Entries take precedence over default rules.
    static ACL_MAP: RefCell<std::collections::HashMap<Principal, AccessLevel>> =
        RefCell::new(std::collections::HashMap::new());
}

pub fn get_acl_entry(principal: &Principal) -> Option<AccessLevel> {
    ACL_MAP.with(|m| m.borrow().get(principal).cloned())
}

pub fn set_acl_entry(principal: Principal, level: AccessLevel) {
    ACL_MAP.with(|m| {
        m.borrow_mut().insert(principal, level);
    });
}

pub fn remove_acl_entry(principal: &Principal) {
    ACL_MAP.with(|m| {
        m.borrow_mut().remove(principal);
    });
}

pub fn list_acl_entries() -> Vec<AclEntry> {
    ACL_MAP.with(|m| {
        m.borrow()
            .iter()
            .map(|(p, l)| AclEntry {
                principal: *p,
                level: l.clone(),
            })
            .collect()
    })
}

// ── Graph alias (§16.2) Graph alias map ──────────────────────────────────────────────────────

thread_local! {
    /// Graph-name aliases for cross-canister routing. Admin-configurable.
    static GRAPH_ALIAS_MAP: RefCell<std::collections::HashMap<String, Principal>> =
        RefCell::new(std::collections::HashMap::new());
}

// ── Registry delegation (§12) Registry principal ──────────────────────────────────────────────────

thread_local! {
    /// Principal of the registry canister used for CREATE/DROP GRAPH delegation.
    static REGISTRY_PRINCIPAL: RefCell<Option<Principal>> = const { RefCell::new(None) };
}

// ── Graph type (§12) Graph type definitions ─────────────────────────────────────────────

/// Scalar element type for typed lists in stored property schemas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, Serialize, Deserialize)]
pub enum StoredScalarType {
    #[serde(alias = "Int")]
    Int64,
    #[serde(alias = "Float")]
    Float64,
    Float32,
    Text,
    Bool,
    Timestamp,
    Bytes,
    Date,
    Time,
    DateTime,
    Duration,
    Principal,
    Decimal,
    #[serde(alias = "Uint")]
    Uint64,
    Int8,
    Int16,
    Int32,
    Int128,
    Int256,
    Uint8,
    Uint16,
    Uint32,
    Uint128,
    Uint256,
}

/// Stored value type for property schema validation (Candid/Serde compatible).
#[derive(Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, Serialize, Deserialize)]
pub enum StoredValueType {
    #[serde(alias = "Int")]
    Int64,
    #[serde(alias = "Float")]
    Float64,
    Float32,
    Text,
    Bool,
    Timestamp,
    List,
    /// Typed list with known element type (e.g. from `LIST<INT>` in DDL).
    TypedList(StoredScalarType),
    Bytes,
    Date,
    Time,
    DateTime,
    Duration,
    Decimal,
    #[serde(alias = "Uint")]
    Uint64,
    Int8,
    Int16,
    Int32,
    Int128,
    Int256,
    Uint8,
    Uint16,
    Uint32,
    Uint128,
    Uint256,
    /// Character string with length constraints.
    TextConstrained {
        min_length: u32,
        max_length: u32,
        fixed: bool,
    },
    /// Byte string with length constraints.
    BytesConstrained {
        min_length: u32,
        max_length: u32,
        fixed: bool,
    },
}

/// A stored property definition within a node type schema.
#[derive(Clone, Debug, PartialEq, candid::CandidType, Serialize, Deserialize)]
pub struct StoredPropertyDef {
    pub name: String,
    pub value_type: StoredValueType,
    pub required: bool,
}

/// A stored node type definition — maps a type name to a set of labels and optional property schema.
#[derive(Clone, Debug, Default, candid::CandidType, Serialize, Deserialize, PartialEq)]
pub struct StoredNodeType {
    pub name: String,
    pub labels: Vec<String>,
    #[serde(default)]
    pub properties: Vec<StoredPropertyDef>,
}

/// A stored edge type definition — maps a type name to endpoint label constraints and optional property schema.
#[derive(Clone, Debug, Default, candid::CandidType, Serialize, Deserialize, PartialEq)]
pub struct StoredEdgeType {
    pub name: String,
    pub label: String,
    pub from_types: Vec<String>,
    pub to_types: Vec<String>,
    #[serde(default)]
    pub properties: Vec<StoredPropertyDef>,
}

/// A stored graph type schema — defines the set of allowed node and edge labels.
#[derive(Clone, Debug, Default, candid::CandidType, Serialize, Deserialize, PartialEq)]
pub struct StoredGraphType {
    pub node_labels: Vec<String>,
    pub edge_labels: Vec<String>,
    #[serde(default)]
    pub node_types: Vec<StoredNodeType>,
    #[serde(default)]
    pub edge_types: Vec<StoredEdgeType>,
}

thread_local! {
    /// Named graph type definitions stored in this canister.
    static GRAPH_TYPE_MAP: RefCell<std::collections::HashMap<String, StoredGraphType>> =
        RefCell::new(std::collections::HashMap::new());
    /// The currently active graph type name (if any). When set, mutations are validated
    /// against the corresponding type's allowed labels.
    static ACTIVE_GRAPH_TYPE: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn get_graph_type(name: &str) -> Option<StoredGraphType> {
    GRAPH_TYPE_MAP.with(|m| m.borrow().get(name).cloned())
}

pub fn set_graph_type(name: String, def: StoredGraphType) {
    GRAPH_TYPE_MAP.with(|m| {
        m.borrow_mut().insert(name, def);
    });
}

pub fn remove_graph_type(name: &str) -> bool {
    GRAPH_TYPE_MAP.with(|m| m.borrow_mut().remove(name).is_some())
}

pub fn list_graph_types() -> Vec<(String, StoredGraphType)> {
    GRAPH_TYPE_MAP.with(|m| {
        m.borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    })
}

pub fn get_active_graph_type_name() -> Option<String> {
    ACTIVE_GRAPH_TYPE.with(|a| a.borrow().clone())
}

pub fn get_active_graph_type() -> Option<StoredGraphType> {
    let name = get_active_graph_type_name()?;
    get_graph_type(&name)
}

pub fn set_active_graph_type(name: Option<String>) {
    ACTIVE_GRAPH_TYPE.with(|a| *a.borrow_mut() = name);
}

/// Resolves a node type name to its label list using the active graph type.
///
/// Returns `None` if no active graph type is set or the type name is not defined.
pub fn resolve_node_type(type_name: &str) -> Option<Vec<String>> {
    let gt = get_active_graph_type()?;
    gt.node_types
        .iter()
        .find(|nt| nt.name.eq_ignore_ascii_case(type_name))
        .map(|nt| nt.labels.clone())
}

// ── Schema (§12) Schema namespaces ──────────────────────────────────────────────────

thread_local! {
    /// Registered schema (namespace) names stored in this canister.
    static SCHEMA_SET: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
}

pub fn create_schema(name: String) -> bool {
    SCHEMA_SET.with(|s| s.borrow_mut().insert(name))
}

pub fn drop_schema(name: &str) -> bool {
    SCHEMA_SET.with(|s| s.borrow_mut().remove(name))
}

pub fn schema_exists(name: &str) -> bool {
    SCHEMA_SET.with(|s| s.borrow().contains(name))
}

// ── §18.9 Phase 3: Strict type checking ───────────────────────────────────

thread_local! {
    /// When `true`, type mismatches are rejected as errors instead of warnings.
    static STRICT_TYPE_CHECK: RefCell<bool> = const { RefCell::new(false) };
}

pub fn is_strict_type_check() -> bool {
    STRICT_TYPE_CHECK.with(|s| *s.borrow())
}

pub fn set_strict_type_check(strict: bool) {
    STRICT_TYPE_CHECK.with(|s| *s.borrow_mut() = strict);
}

// ── §12 Constraint enforcement ────────────────────────────────────────────

/// Kind of schema constraint stored at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, Serialize, Deserialize)]
pub enum StoredConstraintKind {
    Unique,
    NotNull,
}

/// A named schema constraint persisted across upgrades.
#[derive(Clone, Debug, PartialEq, candid::CandidType, Serialize, Deserialize)]
pub struct StoredConstraint {
    pub name: String,
    pub label: String,
    pub property: String,
    pub kind: StoredConstraintKind,
}

thread_local! {
    /// Named constraints stored in this canister.
    static CONSTRAINT_MAP: RefCell<std::collections::HashMap<String, StoredConstraint>> =
        RefCell::new(std::collections::HashMap::new());
}

pub fn get_constraint(name: &str) -> Option<StoredConstraint> {
    CONSTRAINT_MAP.with(|m| m.borrow().get(name).cloned())
}

pub fn set_constraint(name: String, c: StoredConstraint) {
    CONSTRAINT_MAP.with(|m| {
        m.borrow_mut().insert(name, c);
    });
}

pub fn remove_constraint(name: &str) -> bool {
    CONSTRAINT_MAP.with(|m| m.borrow_mut().remove(name).is_some())
}

pub fn list_constraints() -> Vec<StoredConstraint> {
    CONSTRAINT_MAP.with(|m| m.borrow().values().cloned().collect())
}

// ── P5 Prepared statements ────────────────────────────────────────────────

/// Maximum number of prepared statements per canister.
const PREPARED_CACHE_LIMIT: usize = 256;

/// A cached prepared statement.
#[derive(Clone)]
pub struct PreparedStatement {
    pub source: String,
    pub description: Option<String>,
    pub plan: gleaph_gql::plan::PhysicalPlan,
    pub parameters: Vec<gleaph_types::PreparedParameterInfo>,
    pub is_mutation: bool,
    pub stmt: gleaph_gql::ast::Statement,
    pub allowed_sorts: Vec<PreparedSortDef>,
    pub default_sort: Option<Vec<gleaph_types::PreparedSortSpec>>,
    pub type_warnings: Vec<gleaph_types::TypeDiagnostic>,
}

#[derive(Clone)]
pub struct PreparedSortDef {
    pub key: String,
    pub expr_source: String,
    pub expr: gleaph_gql::ast::Expr,
}

#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
struct StoredPreparedStatementSnapshot {
    name: String,
    source: String,
    #[serde(default)]
    options: Option<gleaph_types::PreparedOptions>,
}

thread_local! {
    static PREPARED_CACHE: RefCell<std::collections::HashMap<String, PreparedStatement>> =
        RefCell::new(std::collections::HashMap::new());
}

pub fn prepare_stmt(name: String, ps: PreparedStatement) -> Result<(), GleaphError> {
    PREPARED_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if !cache.contains_key(&name) && cache.len() >= PREPARED_CACHE_LIMIT {
            return Err(GleaphError::ExecutionError(format!(
                "prepared statement cache full (limit {PREPARED_CACHE_LIMIT})"
            )));
        }
        cache.insert(name, ps);
        Ok(())
    })
}

pub fn get_prepared_stmt(name: &str) -> Option<PreparedStatement> {
    PREPARED_CACHE.with(|c| {
        c.borrow().get(name).map(|ps| PreparedStatement {
            source: ps.source.clone(),
            description: ps.description.clone(),
            plan: ps.plan.clone(),
            parameters: ps.parameters.clone(),
            is_mutation: ps.is_mutation,
            stmt: ps.stmt.clone(),
            allowed_sorts: ps.allowed_sorts.clone(),
            default_sort: ps.default_sort.clone(),
            type_warnings: ps.type_warnings.clone(),
        })
    })
}

pub fn drop_prepared_stmt(name: &str) -> bool {
    PREPARED_CACHE.with(|c| c.borrow_mut().remove(name).is_some())
}

/// Returns prepared statement definitions for snapshot persistence.
fn list_prepared_sources() -> Vec<StoredPreparedStatementSnapshot> {
    PREPARED_CACHE.with(|c| {
        c.borrow()
            .iter()
            .map(|(k, v)| StoredPreparedStatementSnapshot {
                name: k.clone(),
                source: v.source.clone(),
                options: Some(gleaph_types::PreparedOptions {
                    description: v.description.clone(),
                    allowed_sorts: v
                        .allowed_sorts
                        .iter()
                        .map(|sort| gleaph_types::PreparedSortKey {
                            key: sort.key.clone(),
                            expr: sort.expr_source.clone(),
                        })
                        .collect(),
                    default_sort: v.default_sort.clone(),
                }),
            })
            .collect()
    })
}

pub fn list_prepared_stmts() -> Vec<(String, PreparedStatement)> {
    PREPARED_CACHE.with(|c| {
        c.borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    })
}

pub fn prepared_cache_len() -> usize {
    PREPARED_CACHE.with(|c| c.borrow().len())
}

/// Re-prepare a statement from its GQL source during restore.
/// Parses, validates, and (for queries) builds a plan, then caches.
fn re_prepare_from_source(
    name: &str,
    source: &str,
    options: Option<gleaph_types::PreparedOptions>,
) -> Result<(), GleaphError> {
    let info = crate::gql_bridge::prepare_statement(name, source, options)?;
    let _ = info;
    Ok(())
}

pub fn list_schemas() -> Vec<String> {
    SCHEMA_SET.with(|s| {
        let mut v: Vec<String> = s.borrow().iter().cloned().collect();
        v.sort();
        v
    })
}

pub fn get_registry_principal() -> Option<Principal> {
    REGISTRY_PRINCIPAL.with(|p| *p.borrow())
}

pub fn set_registry_principal(principal: Principal) {
    REGISTRY_PRINCIPAL.with(|p| *p.borrow_mut() = Some(principal));
}

pub fn resolve_graph_alias(name: &str) -> Option<Principal> {
    GRAPH_ALIAS_MAP.with(|m| m.borrow().get(name).copied())
}

pub fn set_graph_alias(name: String, canister_id: Principal) {
    GRAPH_ALIAS_MAP.with(|m| {
        m.borrow_mut().insert(name, canister_id);
    });
}

pub fn remove_graph_alias(name: &str) -> bool {
    GRAPH_ALIAS_MAP.with(|m| m.borrow_mut().remove(name).is_some())
}

pub fn list_graph_aliases() -> Vec<gleaph_types::GraphAlias> {
    GRAPH_ALIAS_MAP.with(|m| {
        m.borrow()
            .iter()
            .map(|(name, cid)| gleaph_types::GraphAlias {
                name: name.clone(),
                canister_id: *cid,
            })
            .collect()
    })
}

/// Returns the initial vertex capacity configured for this canister instance.
pub fn config_initial_vertex_capacity() -> u32 {
    CONFIG.with(|c| c.borrow().initial_vertex_capacity)
}

#[cfg(test)]
pub fn reset_metrics_and_quota_for_test() {
    METRICS.with(|m| {
        *m.borrow_mut() = OperationalMetrics {
            query_count: 0,
            mutation_count: 0,
            rejected_count: 0,
            algorithm_calls: 0,
            stable_memory_bytes: 0,
        }
    });
    QUOTA.with(|q| {
        *q.borrow_mut() = UsageQuota {
            max_vertices: 0,
            max_edges: 0,
        }
    });
    GRAPH_ALIAS_MAP.with(|m| m.borrow_mut().clear());
    REGISTRY_PRINCIPAL.with(|p| *p.borrow_mut() = None);
    GRAPH_TYPE_MAP.with(|m| m.borrow_mut().clear());
    ACTIVE_GRAPH_TYPE.with(|a| *a.borrow_mut() = None);
    SCHEMA_SET.with(|s| s.borrow_mut().clear());
    ACL_MAP.with(|m| m.borrow_mut().clear());
    PREPARED_CACHE.with(|c| c.borrow_mut().clear());
    STRICT_TYPE_CHECK.with(|s| *s.borrow_mut() = false);
    CONSTRAINT_MAP.with(|m| m.borrow_mut().clear());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Runtime configuration for the graph canister state.
pub struct GraphCanisterConfig {
    pub initial_vertex_capacity: u32,
}

#[derive(Clone, Debug, Default, candid::CandidType, Serialize, Deserialize, PartialEq)]
struct RuntimePersistSnapshot {
    overlay: GraphOverlaySnapshot,
    certification: crate::certification::CertificationCacheSnapshot,
    /// §21.3: Prepared statement sources and options. Plans are re-built on restore.
    #[serde(default)]
    prepared_sources: Vec<StoredPreparedStatementSnapshot>,
}

#[derive(Clone, Debug, Default, candid::CandidType, Serialize, Deserialize, PartialEq)]
struct StableAdminConfigSnapshot {
    #[serde(default)]
    graph_aliases: Vec<(String, candid::Principal)>,
    #[serde(default)]
    registry_principal: Option<candid::Principal>,
    #[serde(default)]
    acl_entries: Vec<(candid::Principal, AccessLevel)>,
    #[serde(default)]
    graph_types: Vec<(String, StoredGraphType)>,
    #[serde(default)]
    active_graph_type: Option<String>,
    #[serde(default)]
    schemas: Vec<String>,
    #[serde(default)]
    strict_type_check: bool,
    #[serde(default)]
    constraints: Vec<StoredConstraint>,
}

/// Executes a closure with shared access to the initialized graph state.
pub fn with_state<R>(f: impl FnOnce(&PmaGraph<IcStableMemory>) -> R) -> R {
    STATE.with(|s| f(s.borrow().as_ref().expect("graph state not initialized")))
}

/// Executes a closure with mutable access to the initialized graph state.
pub fn with_state_mut<R>(f: impl FnOnce(&mut PmaGraph<IcStableMemory>) -> R) -> R {
    STATE.with(|s| {
        f(s.borrow_mut()
            .as_mut()
            .expect("graph state not initialized"))
    })
}

#[derive(Default, Clone)]
/// Stable-memory adapter backed by IC stable memory on wasm and shared in-process bytes on native builds.
pub struct IcStableMemory {
    #[cfg(not(target_arch = "wasm32"))]
    inner: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
}

impl Memory for IcStableMemory {
    fn size_bytes(&self) -> u64 {
        #[cfg(target_arch = "wasm32")]
        {
            ic_cdk::stable::stable_size() * 65_536
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.inner.borrow().len() as u64
        }
    }

    fn grow(&mut self, additional_bytes: u64) -> Result<(), MemoryError> {
        #[cfg(target_arch = "wasm32")]
        {
            if additional_bytes == 0 {
                return Ok(());
            }
            let pages = additional_bytes.div_ceil(65_536);
            ic_cdk::stable::stable_grow(pages)
                .map(|_| ())
                .map_err(|_| MemoryError::GrowOverflow)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let add = usize::try_from(additional_bytes).map_err(|_| MemoryError::GrowOverflow)?;
            let mut inner = self.inner.borrow_mut();
            let new_len = inner
                .len()
                .checked_add(add)
                .ok_or(MemoryError::GrowOverflow)?;
            inner.resize(new_len, 0);
            Ok(())
        }
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        #[cfg(target_arch = "wasm32")]
        {
            ic_cdk::stable::stable_read(offset, dst);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let inner = self.inner.borrow();
            let start = usize::try_from(offset).expect("range overflow");
            let end = start.checked_add(dst.len()).expect("range overflow");
            assert!(end <= inner.len(), "out-of-bounds memory access");
            dst.copy_from_slice(&inner[start..end]);
        }
    }

    fn write(&mut self, offset: u64, src: &[u8]) {
        #[cfg(target_arch = "wasm32")]
        {
            ic_cdk::stable::stable_write(offset, src);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut inner = self.inner.borrow_mut();
            let start = usize::try_from(offset).expect("range overflow");
            let end = start.checked_add(src.len()).expect("range overflow");
            assert!(end <= inner.len(), "out-of-bounds memory access");
            inner[start..end].copy_from_slice(src);
        }
    }
}

/// Restores graph state from stable memory.
pub fn restore_state() -> Result<(), GleaphError> {
    restore_state_inner(true)
}

/// Restores graph state from stable memory, skipping IC certification APIs.
///
/// Use this in contexts where `ic0_certified_data_set` is unavailable
/// (e.g. canbench non-replicated query mode).
pub fn restore_state_uncertified() -> Result<(), GleaphError> {
    restore_state_inner(false)
}

fn restore_state_inner(with_cert: bool) -> Result<(), GleaphError> {
    restore_state_from_memory(IcStableMemory::default(), with_cert)
}

fn restore_state_from_memory(mem: IcStableMemory, with_cert: bool) -> Result<(), GleaphError> {
    let mut graph = PmaGraph::from_stable_memory(mem)?;
    let mut regions_meta = read_regions_meta(&graph.mem)?.unwrap_or_default();
    let mut prepared_sources_to_restore = Vec::new();
    regions_meta.infer_non_pma_base_if_missing(None);
    if let Some(meta) = read_persist_meta(&graph.mem)? {
        set_quota(meta.quota());
        validate_reserved_region_layout(&graph, &meta, &regions_meta)?;
        if let Some(config) = read_stable_admin_config_snapshot(&graph.mem, &regions_meta)? {
            for (name, cid) in config.graph_aliases {
                set_graph_alias(name, cid);
            }
            if let Some(rp) = config.registry_principal {
                set_registry_principal(rp);
            }
            for (principal, level) in config.acl_entries {
                set_acl_entry(principal, level);
            }
            for (name, def) in config.graph_types {
                set_graph_type(name, def);
            }
            if let Some(name) = config.active_graph_type {
                set_active_graph_type(Some(name));
            }
            for name in config.schemas {
                create_schema(name);
            }
            set_strict_type_check(config.strict_type_check);
            for c in config.constraints {
                set_constraint(c.name.clone(), c);
            }
        }
        if let Some(snapshot) = read_runtime_snapshot(&graph.mem, &meta)? {
            let _rng_state = snapshot.overlay.selectivity_rng_state;
            graph.restore_overlay_snapshot(snapshot.overlay)?;
            // If restoring from an old snapshot without PRNG state, seed from IC time.
            #[cfg(target_arch = "wasm32")]
            if _rng_state == 0 {
                graph.seed_rng(ic_cdk::api::time());
            }
            if with_cert {
                crate::certification::restore_caches(snapshot.certification);
            }
            prepared_sources_to_restore = snapshot.prepared_sources;
        } else if with_cert {
            crate::certification::init_certification();
        }
        // Restore vertex tombstones from stable bitset if region exists (overrides overlay).
        if regions_meta.vertex_tombstone_offset > 0 && regions_meta.vertex_tombstone_len > 0 {
            let bs = VertexTombstoneBitset::open(
                graph.mem.clone(),
                regions_meta.vertex_tombstone_offset,
                graph.num_vertices as u32,
            );
            let tombstoned = bs.collect_all();
            graph.restore_tombstoned_vertices_from_set(tombstoned);
        }
        // Restore vertex labels from VertexMetaTable if region exists (overrides overlay).
        if regions_meta.vertex_meta_offset > 0 && regions_meta.vertex_meta_len > 0 {
            let tbl = VertexMetaTable::open(graph.mem.clone(), regions_meta.vertex_meta_offset)
                .map_err(|e| GleaphError::Memory(e.to_string()))?;
            graph.restore_from_abp_vertex_meta(&tbl);
        }
        // Attach live ABP equality index handle if the stable region exists.
        if regions_meta.secondary_index_offset > 0 && regions_meta.secondary_index_len > 0 {
            // Best-effort: if the header is invalid (e.g. first deploy with region), skip.
            let _ = graph.attach_live_eq_index(regions_meta.secondary_index_offset);
        }
        LAST_PERSIST_META.with(|m| *m.borrow_mut() = Some(meta));
        CONFIG.with(|c| {
            *c.borrow_mut() = GraphCanisterConfig {
                initial_vertex_capacity: meta.max_vertices,
            }
        });
    } else {
        LAST_PERSIST_META.with(|m| *m.borrow_mut() = None);
        if with_cert {
            crate::certification::init_certification();
        }
        CONFIG.with(|c| {
            *c.borrow_mut() = GraphCanisterConfig {
                initial_vertex_capacity: graph.vertex_count() as u32,
            }
        });
    }
    STATE.with(|s| *s.borrow_mut() = Some(graph));
    for prepared in prepared_sources_to_restore {
        let StoredPreparedStatementSnapshot {
            name,
            source,
            options,
        } = prepared;
        if let Err(e) = re_prepare_from_source(&name, &source, options) {
            #[cfg(target_arch = "wasm32")]
            ic_cdk::println!("WARN: failed to re-prepare '{}': {}", name, e);
            let _ = e;
        }
    }
    Ok(())
}

/// Restores graph state or initializes a new graph when no valid header exists.
pub fn restore_or_init_state(default_initial_vertex_capacity: u32) -> Result<(), GleaphError> {
    match restore_state() {
        Ok(()) => Ok(()),
        Err(GleaphError::InvalidHeader) => init_state(default_initial_vertex_capacity, 0),
        Err(e) => Err(e),
    }
}

/// Initializes a fresh graph state and persists metadata.
pub fn init_state(
    initial_vertex_capacity: u32,
    initial_edge_capacity: u64,
) -> Result<(), GleaphError> {
    #[allow(unused_mut)]
    let mut graph = PmaGraph::new_with_initial_edge_capacity(
        IcStableMemory::default(),
        initial_vertex_capacity,
        initial_edge_capacity,
    )?;
    #[cfg(target_arch = "wasm32")]
    graph.seed_rng(ic_cdk::api::time());
    // `init_state` creates a fresh graph over the process-global stable memory backend.
    // When re-initializing in the same canister process (e.g. canbench setup queries),
    // stale reserved-region metadata from the previous graph must not be carried forward.
    let mut header = layout::read_header(&graph.mem);
    header._reserved = [0; 4008];
    layout::write_header(&mut graph.mem, &header);
    LAST_PERSIST_META.with(|m| *m.borrow_mut() = None);
    CONFIG.with(|c| {
        *c.borrow_mut() = GraphCanisterConfig {
            initial_vertex_capacity,
        }
    });
    STATE.with(|s| *s.borrow_mut() = Some(graph));
    persist_state_metadata()?;
    Ok(())
}

/// Persists the graph header and graph-canister metadata into stable memory.
pub fn persist_state_metadata() -> Result<(), GleaphError> {
    let cfg = CONFIG.with(|c| *c.borrow());
    let quota = get_quota();
    with_state_mut(|g| {
        g.refresh_selectivity_if_stale();
        let mut regions_meta = read_regions_meta(&g.mem)?.unwrap_or_default();
        let overlay = g.overlay_snapshot();
        let snapshot = RuntimePersistSnapshot {
            overlay,
            certification: crate::certification::snapshot_caches(),
            prepared_sources: list_prepared_sources(),
        };
        let previous_meta =
            read_persist_meta(&g.mem)?.or_else(|| LAST_PERSIST_META.with(|m| *m.borrow()));

        // Allocate and populate new stable-memory regions BEFORE writing
        // the overlay snapshot so that the overlay remains at the tail of
        // memory and can be reused in-place on subsequent persists.
        // ABP tree create/build may grow memory (ensure_size inside
        // write_to_page), so they must run before the overlay.
        {
            let alloc_meta = previous_meta
                .unwrap_or_else(|| GraphPersistMeta::new(cfg.initial_vertex_capacity, 0, 0, 0));
            // Vertex tombstone bitset.
            let needed =
                VertexTombstoneBitset::<IcStableMemory>::bytes_needed(g.num_vertices as u32);
            if needed > 0 {
                if regions_meta.vertex_tombstone_len == 0 {
                    regions_meta = allocate_reserved_non_pma_region(
                        g,
                        &alloc_meta,
                        ReservedRegionKind::VertexTombstone,
                        needed,
                    )?;
                }
                let mut bs = VertexTombstoneBitset::create(
                    g.mem.clone(),
                    regions_meta.vertex_tombstone_offset,
                    g.num_vertices as u32,
                );
                bs.bulk_write_from_set(g.tombstoned_vertex_set());
                g.mem = bs.into_memory();
            }
            // Vertex labels (VertexMetaTable ABP tree).
            {
                use gleaph_pma::abp_tree::{ABP_PAGE_SIZE, ABP_STORE_HEADER_LEN};
                use gleaph_pma::vertex_meta_table::VERTEX_META_MIN_REGION;
                let requested =
                    VERTEX_META_MIN_REGION.max(ABP_STORE_HEADER_LEN + u64::from(ABP_PAGE_SIZE));
                if regions_meta.vertex_meta_len == 0 {
                    regions_meta = allocate_reserved_non_pma_region(
                        g,
                        &alloc_meta,
                        ReservedRegionKind::VertexMeta,
                        requested,
                    )?;
                }
                let tbl = g.build_abp_vertex_meta_snapshot(
                    g.mem.clone(),
                    regions_meta.vertex_meta_offset,
                )?;
                g.mem = tbl.into_memory();
            }
            refresh_abp_region_lengths_from_headers(&g.mem, &mut regions_meta)?;
        }

        let (overlay_offset, overlay_len, overlay_alloc_len) =
            write_runtime_snapshot(&mut g.mem, &snapshot, previous_meta.as_ref())?;
        g.write_header()?;
        let meta = GraphPersistMeta::new_with_quota(
            cfg.initial_vertex_capacity,
            overlay_offset,
            overlay_len,
            overlay_alloc_len,
            quota.clone(),
        );
        let config_snapshot = current_stable_admin_config_snapshot();
        regions_meta =
            write_stable_admin_config_snapshot(g, &config_snapshot, regions_meta, &meta)?;
        if regions_meta.property_store_offset > 0 && regions_meta.property_store_len > 0 {
            let store =
                g.build_abp_property_store(g.mem.clone(), regions_meta.property_store_offset)?;
            g.mem = store.into_memory();
        }
        // Auto-allocate secondary index region if indexes are registered but no region exists.
        if regions_meta.secondary_index_offset == 0
            && g.list_property_indexes().iter().any(|idx| {
                idx.entity_type == EntityType::Vertex
                    && matches!(idx.index_type, IndexType::Equality | IndexType::Range)
            })
        {
            use gleaph_pma::abp_tree::{ABP_PAGE_SIZE, ABP_STORE_HEADER_LEN};
            let requested = ABP_STORE_HEADER_LEN + u64::from(ABP_PAGE_SIZE);
            let alloc_meta = previous_meta
                .unwrap_or_else(|| GraphPersistMeta::new(cfg.initial_vertex_capacity, 0, 0, 0));
            regions_meta = allocate_reserved_non_pma_region(
                g,
                &alloc_meta,
                ReservedRegionKind::SecondaryIndex,
                requested,
            )?;
        }
        if regions_meta.secondary_index_offset > 0 && regions_meta.secondary_index_len > 0 {
            // Always rebuild ABP from in-memory state for correctness across upgrades.
            // Detach live handle first so build_abp_secondary_index starts fresh.
            g.detach_live_eq_index();
            let idx =
                g.build_abp_secondary_index(g.mem.clone(), regions_meta.secondary_index_offset)?;
            g.mem = idx.into_memory();
            let _ = g.attach_live_eq_index(regions_meta.secondary_index_offset);
        }
        refresh_abp_region_lengths_from_headers(&g.mem, &mut regions_meta)?;
        regions_meta.infer_non_pma_base_if_missing(None);
        validate_reserved_region_layout(g, &meta, &regions_meta)?;
        write_persist_meta(&mut g.mem, meta)?;
        write_regions_meta(&mut g.mem, regions_meta)?;
        LAST_PERSIST_META.with(|m| *m.borrow_mut() = Some(meta));
        Ok(())
    })
}

/// Persists the overlay (header + runtime snapshot + metadata) to stable memory,
/// skipping all ABP region rebuilds.
#[cfg(any(
    feature = "bench-ecom",
    feature = "bench-social",
    feature = "bench-timeline"
))]
pub fn persist_overlay_only() -> Result<(), GleaphError> {
    let cfg = CONFIG.with(|c| *c.borrow());
    let quota = get_quota();
    with_state_mut(|g| {
        g.refresh_selectivity_if_stale();
        let overlay = g.overlay_snapshot();
        let snapshot = RuntimePersistSnapshot {
            overlay,
            certification: crate::certification::snapshot_caches(),
            prepared_sources: list_prepared_sources(),
        };
        let previous_meta =
            read_persist_meta(&g.mem)?.or_else(|| LAST_PERSIST_META.with(|m| *m.borrow()));
        let (overlay_offset, overlay_len, overlay_alloc_len) =
            write_runtime_snapshot(&mut g.mem, &snapshot, previous_meta.as_ref())?;
        g.write_header()?;
        let meta = GraphPersistMeta::new_with_quota(
            cfg.initial_vertex_capacity,
            overlay_offset,
            overlay_len,
            overlay_alloc_len,
            quota,
        );
        let mut regions_meta = read_regions_meta(&g.mem)?.unwrap_or_default();
        let config_snapshot = current_stable_admin_config_snapshot();
        regions_meta =
            write_stable_admin_config_snapshot(g, &config_snapshot, regions_meta, &meta)?;
        refresh_abp_region_lengths_from_headers(&g.mem, &mut regions_meta)?;
        // Clear vertex tombstone/meta regions so restore falls back to the
        // overlay data (which is authoritative in the bench-only persist path).
        // Without this, restore would read the stale ABP tree written by the
        // initial `persist_state_metadata()` call (before data was seeded).
        regions_meta.vertex_tombstone_offset = 0;
        regions_meta.vertex_tombstone_len = 0;
        regions_meta.vertex_meta_offset = 0;
        regions_meta.vertex_meta_len = 0;
        regions_meta.infer_non_pma_base_if_missing(None);
        validate_reserved_region_layout(g, &meta, &regions_meta)?;
        write_persist_meta(&mut g.mem, meta)?;
        write_regions_meta(&mut g.mem, regions_meta)?;
        LAST_PERSIST_META.with(|m| *m.borrow_mut() = Some(meta));
        Ok(())
    })
}

/// Returns the current in-memory graph canister configuration.
pub fn current_config() -> GraphCanisterConfig {
    CONFIG.with(|c| *c.borrow())
}

/// Returns the currently persisted reserved non-PMA region metadata (if present/valid).
pub fn current_regions_meta() -> Result<(u64, u64, u64, u64, u64), GleaphError> {
    with_state(|g| {
        let m = read_regions_meta(&g.mem)?.unwrap_or_default();
        Ok((
            m.property_store_offset,
            m.property_store_len,
            m.secondary_index_offset,
            m.secondary_index_len,
            m.non_pma_base,
        ))
    })
}

/// Ensures the reserved secondary-index region metadata is allocated in the stable header.
///
/// This only reserves/records the region (collision-checked) and grows memory if needed; it does
/// not initialize the ABP tree bytes yet.
pub fn ensure_secondary_index_reserved_region(min_len: u64) -> Result<(), GleaphError> {
    let cfg = CONFIG.with(|c| *c.borrow());
    with_state_mut(|g| {
        let previous_meta =
            read_persist_meta(&g.mem)?.or_else(|| LAST_PERSIST_META.with(|m| *m.borrow()));
        let persist = previous_meta
            .unwrap_or_else(|| GraphPersistMeta::new(cfg.initial_vertex_capacity, 0, 0, 0));
        let requested = min_len.max(ABP_STORE_HEADER_LEN + u64::from(ABP_PAGE_SIZE));
        let _ = allocate_reserved_non_pma_region(
            g,
            &persist,
            ReservedRegionKind::SecondaryIndex,
            requested,
        )?;
        Ok(())
    })
}

/// Ensures the reserved secondary-index region exists and has an initialized ABP header.
pub fn ensure_secondary_index_reserved_region_initialized(min_len: u64) -> Result<(), GleaphError> {
    ensure_secondary_index_reserved_region(min_len)?;
    with_state_mut(|g| {
        let mut regions = read_regions_meta(&g.mem)?.unwrap_or_default();
        if regions.secondary_index_offset == 0 || regions.secondary_index_len == 0 {
            return Ok(());
        }
        if AbpStoreHeader::read_from(&g.mem, regions.secondary_index_offset).is_none() {
            let idx = AbpSecondaryEqIndex::new(g.mem.clone(), regions.secondary_index_offset)
                .map_err(|e| {
                    GleaphError::ExecutionError(format!("init secondary ABP region: {e}"))
                })?;
            g.mem = idx.into_memory();
            refresh_abp_region_lengths_from_headers(&g.mem, &mut regions)?;
            write_regions_meta(&mut g.mem, regions)?;
        }
        Ok(())
    })
}

/// Ensures the reserved property-store region metadata is allocated in the stable header.
pub fn ensure_property_store_reserved_region(min_len: u64) -> Result<(), GleaphError> {
    let cfg = CONFIG.with(|c| *c.borrow());
    with_state_mut(|g| {
        let previous_meta =
            read_persist_meta(&g.mem)?.or_else(|| LAST_PERSIST_META.with(|m| *m.borrow()));
        let persist = previous_meta
            .unwrap_or_else(|| GraphPersistMeta::new(cfg.initial_vertex_capacity, 0, 0, 0));
        let requested = min_len.max(ABP_STORE_HEADER_LEN + u64::from(ABP_PAGE_SIZE));
        let _ = allocate_reserved_non_pma_region(
            g,
            &persist,
            ReservedRegionKind::PropertyStore,
            requested,
        )?;
        Ok(())
    })
}

/// Ensures the reserved property-store region exists and has an initialized ABP header.
pub fn ensure_property_store_reserved_region_initialized(min_len: u64) -> Result<(), GleaphError> {
    ensure_property_store_reserved_region(min_len)?;
    with_state_mut(|g| {
        let mut regions = read_regions_meta(&g.mem)?.unwrap_or_default();
        if regions.property_store_offset == 0 || regions.property_store_len == 0 {
            return Ok(());
        }
        if AbpStoreHeader::read_from(&g.mem, regions.property_store_offset).is_none() {
            let store = AbpPropertyStore::new(g.mem.clone(), regions.property_store_offset)
                .map_err(|e| {
                    GleaphError::ExecutionError(format!("init property-store ABP region: {e}"))
                })?;
            g.mem = store.into_memory();
            refresh_abp_region_lengths_from_headers(&g.mem, &mut regions)?;
            write_regions_meta(&mut g.mem, regions)?;
        }
        Ok(())
    })
}

/// Rebuilds the stable `(a,b)+ tree` property-store snapshot into the reserved region.
pub fn rebuild_property_store_abp_snapshot() -> Result<(), GleaphError> {
    with_state_mut(|g| {
        let mut regions = read_regions_meta(&g.mem)?.unwrap_or_default();
        if regions.property_store_offset == 0 || regions.property_store_len == 0 {
            return Ok(());
        }
        let store = g.build_abp_property_store(g.mem.clone(), regions.property_store_offset)?;
        g.mem = store.into_memory();
        refresh_abp_region_lengths_from_headers(&g.mem, &mut regions)?;
        write_regions_meta(&mut g.mem, regions)?;
        Ok(())
    })
}

/// Rebuilds the stable `(a,b)+ tree` secondary equality index snapshot into the reserved region.
///
/// This is a snapshot rebuild (backfill) for currently registered vertex equality indexes.
pub fn rebuild_secondary_index_abp_snapshot() -> Result<(), GleaphError> {
    with_state_mut(|g| {
        let mut regions = read_regions_meta(&g.mem)?.unwrap_or_default();
        if regions.secondary_index_offset == 0 || regions.secondary_index_len == 0 {
            return Ok(());
        }
        let idx = g.build_abp_secondary_index(g.mem.clone(), regions.secondary_index_offset)?;
        g.mem = idx.into_memory();
        refresh_abp_region_lengths_from_headers(&g.mem, &mut regions)?;
        write_regions_meta(&mut g.mem, regions)?;
        Ok(())
    })
}

/// Refreshes reserved ABP region lengths from on-memory ABP headers and writes the updated
/// `GraphRegionsMeta` back to the stable header.
pub fn refresh_reserved_abp_region_lengths() -> Result<(), GleaphError> {
    with_state_mut(|g| {
        let mut regions = read_regions_meta(&g.mem)?.unwrap_or_default();
        refresh_abp_region_lengths_from_headers(&g.mem, &mut regions)?;
        write_regions_meta(&mut g.mem, regions)?;
        Ok(())
    })
}

fn read_persist_meta<M: Memory>(mem: &M) -> Result<Option<GraphPersistMeta>, GleaphError> {
    let header = layout::read_header(mem);
    if header.magic != gleaph_types::STABLE_MAGIC {
        return Ok(None);
    }
    let maybe = GraphPersistMeta::decode(&header._reserved[..GRAPH_META_RESERVED_LEN]);
    Ok(maybe.filter(GraphPersistMeta::is_valid))
}

fn read_regions_meta<M: Memory>(mem: &M) -> Result<Option<GraphRegionsMeta>, GleaphError> {
    let header = layout::read_header(mem);
    if header.magic != gleaph_types::STABLE_MAGIC {
        return Ok(None);
    }
    let end = GRAPH_REGIONS_META_OFFSET + GRAPH_REGIONS_META_LEN;
    let maybe = GraphRegionsMeta::decode(&header._reserved[GRAPH_REGIONS_META_OFFSET..end]);
    Ok(maybe.filter(GraphRegionsMeta::is_valid))
}

fn write_persist_meta<M: Memory>(mem: &mut M, meta: GraphPersistMeta) -> Result<(), GleaphError> {
    let mut header = layout::read_header(mem);
    if header.magic != gleaph_types::STABLE_MAGIC {
        return Err(GleaphError::InvalidHeader);
    }
    header._reserved[..GRAPH_META_RESERVED_LEN].copy_from_slice(&meta.encode());
    layout::write_header(mem, &header);
    Ok(())
}

fn write_regions_meta<M: Memory>(mem: &mut M, meta: GraphRegionsMeta) -> Result<(), GleaphError> {
    let mut header = layout::read_header(mem);
    if header.magic != gleaph_types::STABLE_MAGIC {
        return Err(GleaphError::InvalidHeader);
    }
    let end = GRAPH_REGIONS_META_OFFSET + GRAPH_REGIONS_META_LEN;
    header._reserved[GRAPH_REGIONS_META_OFFSET..end].copy_from_slice(&meta.encode());
    layout::write_header(mem, &header);
    Ok(())
}

fn current_stable_admin_config_snapshot() -> StableAdminConfigSnapshot {
    StableAdminConfigSnapshot {
        graph_aliases: GRAPH_ALIAS_MAP
            .with(|m| m.borrow().iter().map(|(k, v)| (k.clone(), *v)).collect()),
        registry_principal: REGISTRY_PRINCIPAL.with(|p| *p.borrow()),
        acl_entries: ACL_MAP
            .with(|m| m.borrow().iter().map(|(k, v)| (*k, v.clone())).collect()),
        graph_types: GRAPH_TYPE_MAP.with(|m| {
            m.borrow()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }),
        active_graph_type: ACTIVE_GRAPH_TYPE.with(|a| a.borrow().clone()),
        schemas: list_schemas(),
        strict_type_check: is_strict_type_check(),
        constraints: list_constraints(),
    }
}

fn read_stable_admin_config_snapshot<M: Memory>(
    mem: &M,
    regions: &GraphRegionsMeta,
) -> Result<Option<StableAdminConfigSnapshot>, GleaphError> {
    if regions.config_catalog_len == 0 {
        return Ok(None);
    }
    let end = regions
        .config_catalog_offset
        .checked_add(regions.config_catalog_len)
        .ok_or_else(|| GleaphError::ExecutionError("config catalog offset overflow".into()))?;
    if end > mem.size_bytes() {
        return Err(GleaphError::ExecutionError(format!(
            "config catalog out of bounds: offset={} len={} mem_size={}",
            regions.config_catalog_offset,
            regions.config_catalog_len,
            mem.size_bytes()
        )));
    }
    let mut bytes = vec![0u8; regions.config_catalog_len as usize];
    mem.read(regions.config_catalog_offset, &mut bytes);
    let snapshot = decode_one::<StableAdminConfigSnapshot>(&bytes)
        .map_err(|e| GleaphError::ExecutionError(format!("decode config catalog: {e}")))?;
    Ok(Some(snapshot))
}

fn write_stable_admin_config_snapshot<M: Memory>(
    g: &mut PmaGraph<M>,
    snapshot: &StableAdminConfigSnapshot,
    mut regions: GraphRegionsMeta,
    persist_meta: &GraphPersistMeta,
) -> Result<GraphRegionsMeta, GleaphError> {
    let bytes = encode_one(snapshot)
        .map_err(|e| GleaphError::ExecutionError(format!("encode config catalog: {e}")))?;
    let len = u64::try_from(bytes.len())
        .map_err(|_| GleaphError::ExecutionError("config catalog too large".into()))?;

    if regions.config_catalog_len == 0 {
        regions = allocate_reserved_non_pma_region(
            g,
            persist_meta,
            ReservedRegionKind::ConfigCatalog,
            len,
        )?;
    } else if len > regions.config_catalog_len {
        let overlay_end = persist_meta
            .overlay_offset
            .checked_add(u64::from(
                persist_meta.overlay_alloc_len.max(persist_meta.overlay_len),
            ))
            .ok_or_else(|| GleaphError::ExecutionError("overlay region overflow".into()))?;
        let next_free = [
            regions.non_pma_base.max(overlay_end),
            regions
                .property_store_offset
                .saturating_add(regions.property_store_len),
            regions
                .secondary_index_offset
                .saturating_add(regions.secondary_index_len),
            regions
                .vertex_tombstone_offset
                .saturating_add(regions.vertex_tombstone_len),
            regions.vertex_meta_offset.saturating_add(regions.vertex_meta_len),
            regions
                .config_catalog_offset
                .saturating_add(regions.config_catalog_len),
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        regions.config_catalog_offset = next_free;
        regions.config_catalog_len = len;
        ensure_mem_size(&mut g.mem, next_free.saturating_add(len))?;
    }

    ensure_mem_size(
        &mut g.mem,
        regions
            .config_catalog_offset
            .saturating_add(regions.config_catalog_len),
    )?;
    g.mem.write(regions.config_catalog_offset, &bytes);
    Ok(regions)
}

fn ensure_mem_size<M: Memory>(mem: &mut M, required: u64) -> Result<(), GleaphError> {
    let cur = mem.size_bytes();
    if required > cur {
        mem.grow(required - cur)
            .map_err(|e| GleaphError::Memory(e.to_string()))?;
    }
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
fn relocate_non_pma_regions<M: Memory>(
    mem: &mut M,
    new_pma_end: u64,
    mut regions: GraphRegionsMeta,
) -> Result<GraphRegionsMeta, GleaphError> {
    regions.infer_non_pma_base_if_missing(None);
    if regions.non_pma_base == 0 {
        regions.non_pma_base = new_pma_end;
        write_regions_meta(mem, regions)?;
        return Ok(regions);
    }
    if new_pma_end <= regions.non_pma_base {
        write_regions_meta(mem, regions)?;
        return Ok(regions);
    }
    let shift = new_pma_end - regions.non_pma_base;
    let mut regions_to_move = vec![
        (
            "property_store",
            regions.property_store_offset,
            regions.property_store_len,
        ),
        (
            "secondary_index",
            regions.secondary_index_offset,
            regions.secondary_index_len,
        ),
        (
            "vertex_tombstone",
            regions.vertex_tombstone_offset,
            regions.vertex_tombstone_len,
        ),
        (
            "vertex_meta",
            regions.vertex_meta_offset,
            regions.vertex_meta_len,
        ),
        (
            "config_catalog",
            regions.config_catalog_offset,
            regions.config_catalog_len,
        ),
    ];
    regions_to_move.retain(|(_, _, len)| *len > 0);
    regions_to_move.sort_by(|a, b| b.1.cmp(&a.1)); // descending by offset

    for (_, offset, len) in regions_to_move {
        let dst_offset = offset
            .checked_add(shift)
            .ok_or_else(|| GleaphError::ExecutionError("region relocation overflow".into()))?;
        let dst_end = dst_offset
            .checked_add(len)
            .ok_or_else(|| GleaphError::ExecutionError("region relocation overflow".into()))?;
        ensure_mem_size(mem, dst_end)?;
        let mut buf = vec![0u8; len as usize];
        mem.read(offset, &mut buf);
        mem.write(dst_offset, &buf);
        if regions.property_store_offset == offset && regions.property_store_len == len {
            regions.property_store_offset = dst_offset;
        }
        if regions.secondary_index_offset == offset && regions.secondary_index_len == len {
            regions.secondary_index_offset = dst_offset;
        }
        if regions.vertex_tombstone_offset == offset && regions.vertex_tombstone_len == len {
            regions.vertex_tombstone_offset = dst_offset;
        }
        if regions.vertex_meta_offset == offset && regions.vertex_meta_len == len {
            regions.vertex_meta_offset = dst_offset;
        }
        if regions.config_catalog_offset == offset && regions.config_catalog_len == len {
            regions.config_catalog_offset = dst_offset;
        }
    }

    regions.non_pma_base = new_pma_end;
    write_regions_meta(mem, regions)?;
    Ok(regions)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReservedRegionKind {
    PropertyStore,
    SecondaryIndex,
    VertexTombstone,
    VertexMeta,
    ConfigCatalog,
}

#[allow(dead_code)]
fn allocate_reserved_non_pma_region<M: Memory>(
    g: &mut PmaGraph<M>,
    persist_meta: &GraphPersistMeta,
    kind: ReservedRegionKind,
    requested_len: u64,
) -> Result<GraphRegionsMeta, GleaphError> {
    let mut regions = read_regions_meta(&g.mem)?.unwrap_or_default();
    let pma_end =
        layout::total_memory_needed(g.num_vertices, g.elem_capacity, u64::from(g.segment_count));
    regions.infer_non_pma_base_if_missing(None);
    if regions.non_pma_base == 0 {
        regions.non_pma_base = pma_end;
    }
    if regions.non_pma_base < pma_end {
        return Err(GleaphError::ExecutionError(format!(
            "cannot allocate non-PMA region below PMA end: non_pma_base={} pma_end={}",
            regions.non_pma_base, pma_end
        )));
    }
    let overlay_end = persist_meta
        .overlay_offset
        .checked_add(u64::from(
            persist_meta.overlay_alloc_len.max(persist_meta.overlay_len),
        ))
        .ok_or_else(|| GleaphError::ExecutionError("overlay region overflow".into()))?;
    let mut next_free = regions.non_pma_base.max(overlay_end);
    if regions.property_store_len > 0 {
        next_free = next_free.max(
            regions
                .property_store_offset
                .checked_add(regions.property_store_len)
                .ok_or_else(|| GleaphError::ExecutionError("property region overflow".into()))?,
        );
    }
    if regions.secondary_index_len > 0 {
        next_free = next_free.max(
            regions
                .secondary_index_offset
                .checked_add(regions.secondary_index_len)
                .ok_or_else(|| GleaphError::ExecutionError("secondary region overflow".into()))?,
        );
    }
    if regions.vertex_tombstone_len > 0 {
        next_free = next_free.max(
            regions
                .vertex_tombstone_offset
                .checked_add(regions.vertex_tombstone_len)
                .ok_or_else(|| {
                    GleaphError::ExecutionError("vertex_tombstone region overflow".into())
                })?,
        );
    }
    if regions.vertex_meta_len > 0 {
        next_free = next_free.max(
            regions
                .vertex_meta_offset
                .checked_add(regions.vertex_meta_len)
                .ok_or_else(|| GleaphError::ExecutionError("vertex_meta region overflow".into()))?,
        );
    }
    if regions.config_catalog_len > 0 {
        next_free = next_free.max(
            regions
                .config_catalog_offset
                .checked_add(regions.config_catalog_len)
                .ok_or_else(|| {
                    GleaphError::ExecutionError("config_catalog region overflow".into())
                })?,
        );
    }

    match kind {
        ReservedRegionKind::PropertyStore => {
            if regions.property_store_len == 0 {
                regions.property_store_offset = next_free;
            }
            regions.property_store_len = regions.property_store_len.max(requested_len);
        }
        ReservedRegionKind::SecondaryIndex => {
            if regions.secondary_index_len == 0 {
                regions.secondary_index_offset = next_free;
            }
            regions.secondary_index_len = regions.secondary_index_len.max(requested_len);
        }
        ReservedRegionKind::VertexTombstone => {
            if regions.vertex_tombstone_len == 0 {
                regions.vertex_tombstone_offset = next_free;
            }
            regions.vertex_tombstone_len = regions.vertex_tombstone_len.max(requested_len);
        }
        ReservedRegionKind::VertexMeta => {
            if regions.vertex_meta_len == 0 {
                regions.vertex_meta_offset = next_free;
            }
            regions.vertex_meta_len = regions.vertex_meta_len.max(requested_len);
        }
        ReservedRegionKind::ConfigCatalog => {
            if regions.config_catalog_len == 0 {
                regions.config_catalog_offset = next_free;
            }
            regions.config_catalog_len = regions.config_catalog_len.max(requested_len);
        }
    }

    let max_end = [
        regions
            .property_store_offset
            .saturating_add(regions.property_store_len),
        regions
            .secondary_index_offset
            .saturating_add(regions.secondary_index_len),
        regions
            .vertex_tombstone_offset
            .saturating_add(regions.vertex_tombstone_len),
        regions
            .vertex_meta_offset
            .saturating_add(regions.vertex_meta_len),
        regions
            .config_catalog_offset
            .saturating_add(regions.config_catalog_len),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    if max_end != u64::MAX {
        ensure_mem_size(&mut g.mem, max_end)?;
    }

    validate_reserved_region_layout(g, persist_meta, &regions)?;
    write_regions_meta(&mut g.mem, regions)?;
    Ok(regions)
}

#[derive(Clone, Copy, Debug)]
struct RegionSpan {
    start: u64,
    len: u64,
}

impl RegionSpan {
    fn end(self) -> Option<u64> {
        self.start.checked_add(self.len)
    }
}

fn validate_reserved_region_layout<M: Memory>(
    g: &PmaGraph<M>,
    meta: &GraphPersistMeta,
    regions: &GraphRegionsMeta,
) -> Result<(), GleaphError> {
    let pma = RegionSpan {
        start: 0,
        len: layout::total_memory_needed(
            g.num_vertices,
            g.elem_capacity,
            u64::from(g.segment_count),
        ),
    };
    let overlay = RegionSpan {
        start: meta.overlay_offset,
        len: u64::from(meta.overlay_alloc_len.max(meta.overlay_len)),
    };
    let property = RegionSpan {
        start: regions.property_store_offset,
        len: regions.property_store_len,
    };
    let secondary = RegionSpan {
        start: regions.secondary_index_offset,
        len: regions.secondary_index_len,
    };
    let vertex_tombstone = RegionSpan {
        start: regions.vertex_tombstone_offset,
        len: regions.vertex_tombstone_len,
    };
    let vertex_meta = RegionSpan {
        start: regions.vertex_meta_offset,
        len: regions.vertex_meta_len,
    };
    let config_catalog = RegionSpan {
        start: regions.config_catalog_offset,
        len: regions.config_catalog_len,
    };
    let non_pma_base = regions.non_pma_base;

    let spans = [
        ("pma", pma),
        ("overlay", overlay),
        ("property_store", property),
        ("secondary_index", secondary),
        ("vertex_tombstone", vertex_tombstone),
        ("vertex_meta", vertex_meta),
        ("config_catalog", config_catalog),
    ];

    for (name, span) in spans {
        if span.len == 0 {
            continue;
        }
        let Some(end) = span.end() else {
            return Err(GleaphError::ExecutionError(format!(
                "stable-memory region overflow for {name}: start={} len={}",
                span.start, span.len
            )));
        };
        if end < span.start {
            return Err(GleaphError::ExecutionError(format!(
                "invalid stable-memory region for {name}: start={} len={}",
                span.start, span.len
            )));
        }
    }

    for i in 0..spans.len() {
        for j in (i + 1)..spans.len() {
            let (a_name, a) = spans[i];
            let (b_name, b) = spans[j];
            if a.len == 0 || b.len == 0 {
                continue;
            }
            let a_end = a.end().unwrap();
            let b_end = b.end().unwrap();
            let overlap = a.start < b_end && b.start < a_end;
            if overlap {
                return Err(GleaphError::ExecutionError(format!(
                    "stable-memory region collision: {a_name}[{}..{}) overlaps {b_name}[{}..{})",
                    a.start, a_end, b.start, b_end
                )));
            }
        }
    }

    if non_pma_base != 0 {
        if pma.end().unwrap_or(u64::MAX) > non_pma_base {
            return Err(GleaphError::ExecutionError(format!(
                "pma_end exceeds non_pma_base: pma_end={} non_pma_base={}",
                pma.end().unwrap_or(u64::MAX),
                non_pma_base
            )));
        }
        for (name, span) in [
            ("property_store", property),
            ("secondary_index", secondary),
            ("vertex_tombstone", vertex_tombstone),
            ("vertex_meta", vertex_meta),
            ("config_catalog", config_catalog),
        ] {
            if span.len > 0 && span.start < non_pma_base {
                return Err(GleaphError::ExecutionError(format!(
                    "{name} region starts below non_pma_base: start={} non_pma_base={}",
                    span.start, non_pma_base
                )));
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn read_overlay_snapshot<M: Memory>(
    mem: &M,
    meta: &GraphPersistMeta,
) -> Result<Option<GraphOverlaySnapshot>, GleaphError> {
    if meta.overlay_len == 0 {
        return Ok(None);
    }
    let len = u64::from(meta.overlay_len);
    let end = meta
        .overlay_offset
        .checked_add(len)
        .ok_or_else(|| GleaphError::ExecutionError("overlay snapshot offset overflow".into()))?;
    if end > mem.size_bytes() {
        return Err(GleaphError::ExecutionError(format!(
            "overlay snapshot out of bounds: offset={} len={} mem_size={}",
            meta.overlay_offset,
            meta.overlay_len,
            mem.size_bytes()
        )));
    }
    let mut bytes = vec![0u8; meta.overlay_len as usize];
    mem.read(meta.overlay_offset, &mut bytes);
    let snapshot = decode_one::<GraphOverlaySnapshot>(&bytes)
        .map_err(|e| GleaphError::ExecutionError(format!("decode overlay snapshot: {e}")))?;
    Ok(Some(snapshot))
}

fn read_runtime_snapshot<M: Memory>(
    mem: &M,
    meta: &GraphPersistMeta,
) -> Result<Option<RuntimePersistSnapshot>, GleaphError> {
    if meta.overlay_len == 0 {
        return Ok(None);
    }
    let len = u64::from(meta.overlay_len);
    let end = meta
        .overlay_offset
        .checked_add(len)
        .ok_or_else(|| GleaphError::ExecutionError("overlay snapshot offset overflow".into()))?;
    if end > mem.size_bytes() {
        return Err(GleaphError::ExecutionError(format!(
            "overlay snapshot out of bounds: offset={} len={} mem_size={}",
            meta.overlay_offset,
            meta.overlay_len,
            mem.size_bytes()
        )));
    }
    let mut bytes = vec![0u8; meta.overlay_len as usize];
    mem.read(meta.overlay_offset, &mut bytes);
    let snapshot = decode_one::<RuntimePersistSnapshot>(&bytes)
        .map_err(|e| GleaphError::ExecutionError(format!("decode runtime snapshot: {e}")))?;
    Ok(Some(snapshot))
}

#[allow(dead_code)]
fn write_overlay_snapshot<M: Memory>(
    mem: &mut M,
    snapshot: &GraphOverlaySnapshot,
    previous_meta: Option<&GraphPersistMeta>,
) -> Result<(u64, u32, u32), GleaphError> {
    let bytes = encode_one(snapshot)
        .map_err(|e| GleaphError::ExecutionError(format!("encode overlay snapshot: {e}")))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| GleaphError::ExecutionError("overlay snapshot too large".into()))?;
    let current_size = mem.size_bytes();

    if let Some(prev) = previous_meta {
        let prev_alloc_len = u64::from(prev.overlay_alloc_len.max(prev.overlay_len));
        if prev.overlay_len > 0
            && prev
                .overlay_offset
                .checked_add(prev_alloc_len)
                .is_some_and(|end| end == current_size)
        {
            let offset = prev.overlay_offset;
            if u64::from(len) > prev_alloc_len {
                mem.grow(u64::from(len) - prev_alloc_len)
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
            }
            mem.write(offset, &bytes);
            let alloc_len = prev_alloc_len.max(u64::from(len));
            let alloc_len = u32::try_from(alloc_len).map_err(|_| {
                GleaphError::ExecutionError("overlay snapshot allocation too large".into())
            })?;
            return Ok((offset, len, alloc_len));
        }
    }

    // Fallback: append a fresh snapshot at the current tail when the previous region is no longer
    // the tail (PMA growth may have repurposed that space).
    let offset = current_size;
    mem.grow(u64::from(len))
        .map_err(|e| GleaphError::Memory(e.to_string()))?;
    mem.write(offset, &bytes);
    // Re-read size_bytes after grow: on IC, stable_grow rounds up to 64 KiB page boundaries, so
    // the actual allocation may be larger than `len`.  Record the true post-grow size so that the
    // next persist_state_metadata() tail-check (overlay_offset + overlay_alloc_len == mem.size_bytes())
    // succeeds and we reuse the slot rather than appending again.
    let alloc_len = mem.size_bytes() - offset;
    let alloc_len = u32::try_from(alloc_len)
        .map_err(|_| GleaphError::ExecutionError("overlay snapshot allocation too large".into()))?;
    Ok((offset, len, alloc_len))
}

fn write_runtime_snapshot<M: Memory>(
    mem: &mut M,
    snapshot: &RuntimePersistSnapshot,
    previous_meta: Option<&GraphPersistMeta>,
) -> Result<(u64, u32, u32), GleaphError> {
    let bytes = encode_one(snapshot)
        .map_err(|e| GleaphError::ExecutionError(format!("encode runtime snapshot: {e}")))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| GleaphError::ExecutionError("runtime snapshot too large".into()))?;
    let current_size = mem.size_bytes();

    if let Some(prev) = previous_meta {
        let prev_alloc_len = u64::from(prev.overlay_alloc_len.max(prev.overlay_len));
        if prev.overlay_len > 0
            && prev
                .overlay_offset
                .checked_add(prev_alloc_len)
                .is_some_and(|end| end == current_size)
        {
            let offset = prev.overlay_offset;
            if u64::from(len) > prev_alloc_len {
                mem.grow(u64::from(len) - prev_alloc_len)
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
            }
            mem.write(offset, &bytes);
            let alloc_len = prev_alloc_len.max(u64::from(len));
            let alloc_len = u32::try_from(alloc_len).map_err(|_| {
                GleaphError::ExecutionError("runtime snapshot allocation too large".into())
            })?;
            return Ok((offset, len, alloc_len));
        }
    }

    let offset = current_size;
    mem.grow(u64::from(len))
        .map_err(|e| GleaphError::Memory(e.to_string()))?;
    mem.write(offset, &bytes);
    let alloc_len = mem.size_bytes() - offset;
    let alloc_len = u32::try_from(alloc_len)
        .map_err(|_| GleaphError::ExecutionError("runtime snapshot allocation too large".into()))?;
    Ok((offset, len, alloc_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use gleaph_pma::region_manager::GRAPH_META_MAGIC;
    use gleaph_types::{
        PreparedOptions, PreparedSortKey, PreparedSortSpec, SsspResult, UsageQuota, Value,
    };

    #[derive(Debug, PartialEq)]
    struct RuntimeInventory {
        aliases: Vec<(String, Principal)>,
        registry_principal: Option<Principal>,
        graph_types: Vec<(String, StoredGraphType)>,
        active_graph_type: Option<String>,
        schemas: Vec<String>,
        acl_entries: Vec<(Principal, AccessLevel)>,
        prepared_sources: Vec<(String, String, Option<PreparedOptions>)>,
        strict_type_check: bool,
        constraints: Vec<StoredConstraint>,
        certification: crate::certification::CertificationCacheSnapshot,
        initial_vertex_capacity: u32,
    }

    fn clear_runtime_state_for_restore_test() {
        reset_metrics_and_quota_for_test();
        STATE.with(|s| *s.borrow_mut() = None);
        LAST_PERSIST_META.with(|m| *m.borrow_mut() = None);
        CONFIG.with(|c| {
            *c.borrow_mut() = GraphCanisterConfig {
                initial_vertex_capacity: 1024,
            }
        });
        crate::certification::init_certification();
    }

    fn sorted_acl_entries() -> Vec<(Principal, AccessLevel)> {
        let mut entries: Vec<_> =
            ACL_MAP.with(|m| m.borrow().iter().map(|(p, l)| (*p, l.clone())).collect());
        entries.sort_by(|a, b| a.0.as_slice().cmp(b.0.as_slice()));
        entries
    }

    fn sorted_prepared_sources() -> Vec<(String, String, Option<PreparedOptions>)> {
        let mut prepared: Vec<_> = list_prepared_sources()
            .into_iter()
            .map(|entry| (entry.name, entry.source, entry.options))
            .collect();
        prepared.sort_by(|a, b| a.0.cmp(&b.0));
        prepared
    }

    fn sorted_constraints() -> Vec<StoredConstraint> {
        let mut constraints = list_constraints();
        constraints.sort_by(|a, b| a.name.cmp(&b.name));
        constraints
    }

    fn sorted_graph_types() -> Vec<(String, StoredGraphType)> {
        let mut graph_types = list_graph_types();
        graph_types.sort_by(|a, b| a.0.cmp(&b.0));
        graph_types
    }

    fn sorted_aliases() -> Vec<(String, Principal)> {
        let mut aliases: Vec<_> = list_graph_aliases()
            .into_iter()
            .map(|alias| (alias.name, alias.canister_id))
            .collect();
        aliases.sort_by(|a, b| a.0.cmp(&b.0));
        aliases
    }

    fn collect_runtime_inventory() -> RuntimeInventory {
        RuntimeInventory {
            aliases: sorted_aliases(),
            registry_principal: get_registry_principal(),
            graph_types: sorted_graph_types(),
            active_graph_type: get_active_graph_type_name(),
            schemas: list_schemas(),
            acl_entries: sorted_acl_entries(),
            prepared_sources: sorted_prepared_sources(),
            strict_type_check: is_strict_type_check(),
            constraints: sorted_constraints(),
            certification: crate::certification::snapshot_caches(),
            initial_vertex_capacity: current_config().initial_vertex_capacity,
        }
    }

    fn build_runtime_persistence_fixture() {
        clear_runtime_state_for_restore_test();
        init_state(32, 0).expect("init");

        crate::api::mutate_gql("INSERT (:User {name: 'Alice', age: 30})".into(), None)
            .expect("insert alice");
        crate::api::mutate_gql("INSERT (:User {name: 'Bob', age: 20})".into(), None)
            .expect("insert bob");

        set_graph_alias(
            "analytics".into(),
            Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("analytics principal"),
        );
        set_graph_alias(
            "billing".into(),
            Principal::from_text("ryjl3-tyaaa-aaaaa-aaaba-cai").expect("billing principal"),
        );
        set_registry_principal(
            Principal::from_text("r7inp-6aaaa-aaaaa-aaabq-cai").expect("registry principal"),
        );

        set_graph_type(
            "Social".into(),
            StoredGraphType {
                node_labels: vec!["Audit".into(), "User".into()],
                edge_labels: vec!["KNOWS".into()],
                node_types: vec![StoredNodeType {
                    name: "UserType".into(),
                    labels: vec!["User".into()],
                    properties: vec![StoredPropertyDef {
                        name: "name".into(),
                        value_type: StoredValueType::Text,
                        required: true,
                    }],
                }],
                edge_types: vec![StoredEdgeType {
                    name: "KnowsType".into(),
                    label: "KNOWS".into(),
                    from_types: vec!["UserType".into()],
                    to_types: vec!["UserType".into()],
                    properties: vec![],
                }],
            },
        );
        set_active_graph_type(Some("Social".into()));

        create_schema("analytics".into());
        create_schema("billing".into());

        set_acl_entry(
            Principal::from_text("2vxsx-fae").expect("alice ACL principal"),
            AccessLevel::Write,
        );
        set_acl_entry(
            Principal::from_text("rdmx6-jaaaa-aaaaa-aaadq-cai").expect("bob ACL principal"),
            AccessLevel::Execute,
        );

        crate::gql_bridge::prepare_statement(
            "user_names",
            "MATCH (u:User) RETURN u.name AS name, u.age AS age",
            Some(PreparedOptions {
                description: Some("Sorted user listing".into()),
                allowed_sorts: vec![PreparedSortKey {
                    key: "age".into(),
                    expr: "u.age".into(),
                }],
                default_sort: Some(vec![PreparedSortSpec {
                    key: "age".into(),
                    descending: true,
                    nulls_first: None,
                }]),
            }),
        )
        .expect("prepare user_names");
        crate::gql_bridge::prepare_statement(
            "insert_audit",
            "INSERT (:Audit {name: 'persisted'})",
            None,
        )
        .expect("prepare insert_audit");

        set_strict_type_check(true);
        set_constraint(
            "uniq_user_name".into(),
            StoredConstraint {
                name: "uniq_user_name".into(),
                label: "User".into(),
                property: "name".into(),
                kind: StoredConstraintKind::Unique,
            },
        );

        crate::certification::certify_pagerank(
            b"pr-cache".to_vec(),
            gleaph_types::PageRankResult {
                scores: vec![(0, 1.0), (1, 0.5)],
                iterations: 4,
                converged: true,
            },
        );
        crate::certification::cache_sssp_result(
            b"sssp-cache".to_vec(),
            &SsspResult {
                distances: vec![(0, 0.0), (1, 1.0)],
                predecessors: vec![(0, None), (1, Some(0))],
            },
        );
    }

    fn restore_runtime_state_from_snapshot(
        snapshot: RuntimePersistSnapshot,
        config: StableAdminConfigSnapshot,
        with_cert: bool,
    ) {
        reset_metrics_and_quota_for_test();
        if with_cert {
            crate::certification::restore_caches(snapshot.certification);
        } else {
            crate::certification::init_certification();
        }
        for (name, cid) in config.graph_aliases {
            set_graph_alias(name, cid);
        }
        if let Some(registry_principal) = config.registry_principal {
            set_registry_principal(registry_principal);
        }
        for (principal, level) in config.acl_entries {
            set_acl_entry(principal, level);
        }
        for (name, def) in config.graph_types {
            set_graph_type(name, def);
        }
        set_active_graph_type(config.active_graph_type);
        for name in config.schemas {
            create_schema(name);
        }
        set_strict_type_check(config.strict_type_check);
        for constraint in config.constraints {
            set_constraint(constraint.name.clone(), constraint);
        }
        for prepared in snapshot.prepared_sources {
            let StoredPreparedStatementSnapshot {
                name,
                source,
                options,
            } = prepared;
            re_prepare_from_source(&name, &source, options).expect("re-prepare restored statement");
        }
    }

    fn assert_runtime_behavior_after_snapshot_replay() {
        let result = crate::gql_bridge::execute_prepared_query("user_names", &HashMap::new(), None)
            .expect("execute restored prepared query");
        assert_eq!(
            result.rows,
            vec![
                vec![Value::Text("Alice".into()), Value::Int32(30)],
                vec![Value::Text("Bob".into()), Value::Int32(20)],
            ]
        );

        let outcome = crate::gql_bridge::execute_prepared_mutation("insert_audit", &HashMap::new())
            .expect("execute restored prepared mutation");
        assert_eq!(outcome.result.affected_vertices, 1);

        let audit_rows = crate::api::query_gql("MATCH (n:Audit) RETURN n.name".into(), None)
            .expect("query audit");
        assert_eq!(
            audit_rows.result.rows,
            vec![vec![Value::Text("persisted".into())]]
        );

        let strict_err = crate::gql_bridge::prepare_statement(
            "strict_probe",
            "MATCH (u:User) WHERE 42 = $x AND 'hello' = $x RETURN u",
            None,
        )
        .expect_err("strict mode should still be active");
        assert!(strict_err.to_string().contains("strict type check"));

        let graph_type_err =
            crate::api::mutate_gql("INSERT (:Forbidden {name: 'nope'})".into(), None)
                .expect_err("active graph type should reject unknown labels");
        assert!(matches!(graph_type_err, GleaphError::ValidationError(_)));

        let constraint_err =
            crate::api::mutate_gql("INSERT (:User {name: 'Alice', age: 99})".into(), None)
                .expect_err("unique constraint should survive restore");
        assert!(constraint_err.to_string().contains("UNIQUE constraint"));
    }

    #[test]
    fn metadata_round_trip() {
        let mut mem = IcStableMemory::default();
        let mut graph = PmaGraph::new(mem, 16).expect("init graph");
        graph.insert(0, 1, 0, 1.0, 123).expect("insert");
        graph.write_header().expect("write header");
        write_persist_meta(&mut graph.mem, GraphPersistMeta::new(16, 0, 0, 0)).expect("write meta");
        mem = graph.mem.clone();

        let restored = PmaGraph::from_stable_memory(mem.clone()).expect("restore graph");
        assert_eq!(restored.vertex_count(), 16);
        assert_eq!(restored.edge_count(), 1);

        let meta = read_persist_meta(&mem)
            .expect("read meta")
            .expect("meta present");
        assert_eq!(meta.max_vertices, 16);
        assert_eq!(meta.quota(), UsageQuota::default());
        assert!(meta.is_valid());

        let regions = read_regions_meta(&mem)
            .expect("read regions")
            .unwrap_or_default();
        assert!(regions.is_valid());
    }

    #[test]
    fn decode_metadata_round_trips_quota_fields() {
        let mut bytes = [0u8; GRAPH_META_RESERVED_LEN];
        bytes[0..4].copy_from_slice(&GRAPH_META_MAGIC.to_le_bytes());
        bytes[4..6].copy_from_slice(&GraphPersistMeta::new(16, 0, 0, 0).version.to_le_bytes());
        bytes[6..8].copy_from_slice(&0u16.to_le_bytes());
        bytes[8..12].copy_from_slice(&16u32.to_le_bytes());
        bytes[12..20].copy_from_slice(&0xDEADBEEFCAFEBABEu64.to_le_bytes());
        bytes[20..24].copy_from_slice(&0xA5A5_A5A5u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&0xFFFF_0001u32.to_le_bytes());
        bytes[28..36].copy_from_slice(&123u64.to_le_bytes());
        bytes[36..44].copy_from_slice(&456u64.to_le_bytes());

        let meta = GraphPersistMeta::decode(&bytes).expect("decode meta");
        assert!(meta.is_valid());
        assert_eq!(meta.max_vertices, 16);
        assert_eq!(meta.overlay_offset, 0xDEADBEEFCAFEBABE);
        assert_eq!(meta.overlay_len, 0xA5A5_A5A5);
        assert_eq!(meta.overlay_alloc_len, 0xFFFF_0001);
        assert_eq!(
            meta.quota(),
            UsageQuota {
                max_vertices: 123,
                max_edges: 456,
            }
        );
    }

    #[test]
    fn read_overlay_snapshot_rejects_out_of_bounds_metadata() {
        let mem = IcStableMemory::default();
        let meta = GraphPersistMeta::new(16, 1024, 32, 32);
        let err = read_overlay_snapshot(&mem, &meta).expect_err("oob overlay should error");
        assert!(matches!(err, GleaphError::ExecutionError(_)));
    }

    #[test]
    fn write_overlay_snapshot_reuses_tail_region() {
        let mut graph = PmaGraph::new(IcStableMemory::default(), 4).expect("graph");
        let snapshot = graph.overlay_snapshot();

        let (offset1, len1, alloc1) =
            write_overlay_snapshot(&mut graph.mem, &snapshot, None).expect("first write");
        let size_after_first = graph.mem.size_bytes();

        let prev = GraphPersistMeta::new(4, offset1, len1, alloc1);
        let (offset2, _len2, _alloc2) =
            write_overlay_snapshot(&mut graph.mem, &snapshot, Some(&prev)).expect("second write");

        assert_eq!(offset2, offset1);
        assert_eq!(graph.mem.size_bytes(), size_after_first);
    }

    #[test]
    fn write_overlay_snapshot_reuses_tail_region_after_shrink() {
        let mut graph = PmaGraph::new(IcStableMemory::default(), 4).expect("graph");
        let large = GraphOverlaySnapshot {
            vertex_labels: vec![(1, vec!["User".into(), "Admin".into()])],
            ..Default::default()
        };
        let small = GraphOverlaySnapshot::default();

        let (offset1, len1, alloc1) =
            write_overlay_snapshot(&mut graph.mem, &large, None).expect("first write");
        let size_after_first = graph.mem.size_bytes();

        let prev1 = GraphPersistMeta::new(4, offset1, len1, alloc1);
        let (offset2, len2, alloc2) =
            write_overlay_snapshot(&mut graph.mem, &small, Some(&prev1)).expect("shrink write");
        assert_eq!(offset2, offset1);
        assert!(len2 <= len1);
        assert_eq!(alloc2, alloc1);
        assert_eq!(graph.mem.size_bytes(), size_after_first);

        let prev2 = GraphPersistMeta::new(4, offset2, len2, alloc2);
        let (offset3, _len3, _alloc3) =
            write_overlay_snapshot(&mut graph.mem, &small, Some(&prev2))
                .expect("reuse after shrink");
        assert_eq!(offset3, offset1);
        assert_eq!(graph.mem.size_bytes(), size_after_first);
    }

    #[test]
    fn persist_state_metadata_reuses_overlay_region_after_header_rewrite() {
        init_state(4, 0).expect("init");
        let overlay_alloc_before = with_state(|g| {
            let meta = read_persist_meta(&g.mem)
                .expect("read persist meta")
                .expect("persist meta present");
            meta.overlay_alloc_len
        });

        with_state_mut(|g| {
            g.write_header()
                .expect("rewrite header without persist meta")
        });
        persist_state_metadata().expect("persist with runtime meta fallback");

        let overlay_alloc_after = with_state(|g| {
            let meta = read_persist_meta(&g.mem)
                .expect("read persist meta")
                .expect("persist meta present");
            meta.overlay_alloc_len
        });
        assert_eq!(overlay_alloc_after, overlay_alloc_before);
    }

    #[test]
    fn regions_meta_round_trip() {
        let mut graph = PmaGraph::new(IcStableMemory::default(), 8).expect("graph");
        graph.write_header().expect("header");
        let regions = GraphRegionsMeta {
            property_store_offset: 1 << 20,
            property_store_len: 1 << 16,
            secondary_index_offset: (1 << 20) + (1 << 16),
            secondary_index_len: 1 << 15,
            non_pma_base: 1 << 20,
            ..Default::default()
        };
        write_regions_meta(&mut graph.mem, regions).expect("write regions");
        let read = read_regions_meta(&graph.mem)
            .expect("read regions")
            .expect("present");
        assert_eq!(read, regions);
    }

    #[test]
    fn validate_reserved_region_layout_rejects_overlap() {
        let graph = PmaGraph::new(IcStableMemory::default(), 8).expect("graph");
        let pma_end = layout::total_memory_needed(
            graph.num_vertices,
            graph.elem_capacity,
            u64::from(graph.segment_count),
        );
        let meta = GraphPersistMeta::new(8, pma_end + 1024, 128, 128);
        let regions = GraphRegionsMeta {
            property_store_offset: pma_end + 1100, // overlaps overlay region
            property_store_len: 256,
            ..Default::default()
        };
        let err = validate_reserved_region_layout(&graph, &meta, &regions)
            .expect_err("overlap should fail");
        assert!(matches!(err, GleaphError::ExecutionError(_)));
    }

    #[test]
    fn validate_reserved_region_layout_rejects_pma_non_pma_overlap() {
        let graph = PmaGraph::new(IcStableMemory::default(), 8).expect("graph");
        let pma_end = layout::total_memory_needed(
            graph.num_vertices,
            graph.elem_capacity,
            u64::from(graph.segment_count),
        );
        let meta = GraphPersistMeta::new(8, 0, 0, 0);
        let regions = GraphRegionsMeta {
            property_store_offset: pma_end.saturating_sub(64),
            property_store_len: 256,
            non_pma_base: pma_end + 1024,
            ..Default::default()
        };
        let err = validate_reserved_region_layout(&graph, &meta, &regions)
            .expect_err("PMA/non-PMA overlap should fail");
        assert!(matches!(err, GleaphError::ExecutionError(_)));
    }

    #[test]
    fn regions_meta_infers_non_pma_base_from_existing_regions() {
        let mut regions = GraphRegionsMeta {
            property_store_offset: 10_000,
            property_store_len: 500,
            secondary_index_offset: 20_000,
            secondary_index_len: 500,
            non_pma_base: 0,
            ..Default::default()
        };
        regions.infer_non_pma_base_if_missing(None);
        assert_eq!(regions.non_pma_base, 10_000);
    }

    #[test]
    fn relocate_non_pma_regions_moves_bytes_and_updates_metadata() {
        let mut graph = PmaGraph::new(IcStableMemory::default(), 8).expect("graph");
        graph.write_header().expect("header");
        let pma_end = layout::total_memory_needed(
            graph.num_vertices,
            graph.elem_capacity,
            u64::from(graph.segment_count),
        );

        let regions = GraphRegionsMeta {
            property_store_offset: pma_end + 1024,
            property_store_len: 16,
            secondary_index_offset: pma_end + 4096,
            secondary_index_len: 8,
            non_pma_base: pma_end + 512,
            ..Default::default()
        };
        write_regions_meta(&mut graph.mem, regions).expect("write regions");
        let need = regions.secondary_index_offset + regions.secondary_index_len;
        if graph.mem.size_bytes() < need {
            graph
                .mem
                .grow(need - graph.mem.size_bytes())
                .expect("grow for region writes");
        }
        graph
            .mem
            .write(regions.property_store_offset, b"abcdefghijklmnop");
        graph.mem.write(regions.secondary_index_offset, b"ABCDEFGH");

        let moved = relocate_non_pma_regions(&mut graph.mem, regions.non_pma_base + 2048, regions)
            .expect("relocate");

        assert_eq!(moved.non_pma_base, regions.non_pma_base + 2048);
        assert_eq!(
            moved.property_store_offset,
            regions.property_store_offset + 2048
        );
        assert_eq!(
            moved.secondary_index_offset,
            regions.secondary_index_offset + 2048
        );

        let mut p = [0u8; 16];
        graph.mem.read(moved.property_store_offset, &mut p);
        assert_eq!(&p, b"abcdefghijklmnop");
        let mut s = [0u8; 8];
        graph.mem.read(moved.secondary_index_offset, &mut s);
        assert_eq!(&s, b"ABCDEFGH");

        let persisted = read_regions_meta(&graph.mem)
            .expect("read")
            .expect("present");
        assert_eq!(persisted, moved);
    }

    #[test]
    fn relocate_non_pma_regions_fast_path_skips_copy_when_no_overlap() {
        let mut graph = PmaGraph::new(IcStableMemory::default(), 8).expect("graph");
        graph.write_header().expect("header");
        let pma_end = layout::total_memory_needed(
            graph.num_vertices,
            graph.elem_capacity,
            u64::from(graph.segment_count),
        );
        let regions = GraphRegionsMeta {
            property_store_offset: pma_end + 4096,
            property_store_len: 4,
            non_pma_base: pma_end + 1024,
            ..Default::default()
        };
        write_regions_meta(&mut graph.mem, regions).expect("write regions");
        let need = regions.property_store_offset + regions.property_store_len;
        if graph.mem.size_bytes() < need {
            graph
                .mem
                .grow(need - graph.mem.size_bytes())
                .expect("grow for region writes");
        }
        graph.mem.write(regions.property_store_offset, b"ABCD");
        let out = relocate_non_pma_regions(&mut graph.mem, pma_end, regions).expect("fast path");
        assert_eq!(out, regions);
        let mut buf = [0u8; 4];
        graph.mem.read(regions.property_store_offset, &mut buf);
        assert_eq!(&buf, b"ABCD");
    }

    #[test]
    fn allocate_reserved_non_pma_region_sets_non_pma_base_from_pma_end() {
        let mut graph = PmaGraph::new(IcStableMemory::default(), 8).expect("graph");
        graph.write_header().expect("header");
        let pma_end = layout::total_memory_needed(
            graph.num_vertices,
            graph.elem_capacity,
            u64::from(graph.segment_count),
        );
        let persist = GraphPersistMeta::new(8, 0, 0, 0);

        let out = allocate_reserved_non_pma_region(
            &mut graph,
            &persist,
            ReservedRegionKind::PropertyStore,
            4096,
        )
        .expect("allocate property region");

        assert_eq!(out.non_pma_base, pma_end);
        assert_eq!(out.property_store_offset, pma_end);
        assert_eq!(out.property_store_len, 4096);

        let read = read_regions_meta(&graph.mem)
            .expect("read")
            .expect("present");
        assert_eq!(read, out);
    }

    #[test]
    fn allocate_reserved_non_pma_region_places_secondary_after_property() {
        let mut graph = PmaGraph::new(IcStableMemory::default(), 8).expect("graph");
        graph.write_header().expect("header");
        let persist = GraphPersistMeta::new(8, 0, 0, 0);

        let property = allocate_reserved_non_pma_region(
            &mut graph,
            &persist,
            ReservedRegionKind::PropertyStore,
            8192,
        )
        .expect("alloc property");
        let out = allocate_reserved_non_pma_region(
            &mut graph,
            &persist,
            ReservedRegionKind::SecondaryIndex,
            2048,
        )
        .expect("alloc secondary");

        assert_eq!(
            out.secondary_index_offset,
            property.property_store_offset + property.property_store_len
        );
        assert_eq!(out.secondary_index_len, 2048);
        assert_eq!(out.non_pma_base, property.non_pma_base);
    }

    #[test]
    fn refresh_abp_region_lengths_reads_allocated_extent_from_headers() {
        let mut graph = PmaGraph::new(IcStableMemory::default(), 8).expect("graph");
        graph.write_header().expect("header");
        let pma_end = layout::total_memory_needed(
            graph.num_vertices,
            graph.elem_capacity,
            u64::from(graph.segment_count),
        );
        let property_offset = pma_end + 4096;
        let secondary_offset = property_offset + 64 * 1024;

        let property_hdr = AbpStoreHeader {
            page_size: 4096,
            next_page_id: 3,
            ..Default::default()
        };
        property_hdr
            .write_to(&mut graph.mem, property_offset)
            .expect("write property hdr");
        let secondary_hdr = AbpStoreHeader {
            page_size: 4096,
            next_page_id: 5,
            ..Default::default()
        };
        secondary_hdr
            .write_to(&mut graph.mem, secondary_offset)
            .expect("write secondary hdr");

        let mut regions = GraphRegionsMeta {
            property_store_offset: property_offset,
            property_store_len: 1, // stale
            secondary_index_offset: secondary_offset,
            secondary_index_len: 1, // stale
            non_pma_base: property_offset,
            ..Default::default()
        };
        refresh_abp_region_lengths_from_headers(&graph.mem, &mut regions).expect("refresh");

        assert_eq!(regions.property_store_len, ABP_STORE_HEADER_LEN + 3 * 4096);
        assert_eq!(regions.secondary_index_len, ABP_STORE_HEADER_LEN + 5 * 4096);
    }

    #[test]
    fn rebuild_property_store_abp_snapshot_writes_reserved_region() {
        use gleaph_pma::AbpPropertyStore;
        use gleaph_types::Value;

        init_state(16, 0).expect("init");
        with_state_mut(|g| {
            let v = g
                .create_vertex(
                    vec!["User".into()],
                    vec![("name".into(), Value::Text("A".into()))],
                )
                .expect("create");
            g.set_vertex_prop(v, "age".to_string(), Value::Int64(20))
                .expect("set");
        });
        ensure_property_store_reserved_region(0).expect("reserve property region");
        rebuild_property_store_abp_snapshot().expect("rebuild property snapshot");

        let (prop_off, prop_len, _sec_off, _sec_len, _base) =
            current_regions_meta().expect("regions meta");
        assert!(prop_off > 0);
        assert!(prop_len > 0);
        with_state(|g| {
            let hdr = AbpStoreHeader::read_from(&g.mem, prop_off).expect("abp property hdr");
            assert!(hdr.next_page_id >= 1);
            let store = AbpPropertyStore::from_memory(g.mem.clone(), prop_off).expect("open store");
            let hits = (0..g.vertex_count() as u32)
                .find_map(|vid| store.get_vertex_prop(vid, "name").map(|v| (vid, v)))
                .expect("find name prop");
            assert_eq!(hits.1, Value::Text("A".into()));
            assert_eq!(store.get_vertex_prop(hits.0, "age"), Some(Value::Int64(20)));
        });
    }

    #[test]
    fn ensure_property_store_reserved_region_initialized_writes_abp_header() {
        init_state(16, 0).expect("init");
        ensure_property_store_reserved_region_initialized(0).expect("ensure+init property region");
        let (prop_off, prop_len, _sec_off, _sec_len, _base) = current_regions_meta().expect("meta");
        assert!(prop_off > 0);
        assert!(prop_len >= ABP_STORE_HEADER_LEN + u64::from(ABP_PAGE_SIZE));
        with_state(|g| {
            let hdr = AbpStoreHeader::read_from(&g.mem, prop_off).expect("abp hdr");
            assert_eq!(hdr.page_size, ABP_PAGE_SIZE);
        });
    }

    #[test]
    fn ensure_secondary_index_reserved_region_initialized_writes_abp_header() {
        init_state(16, 0).expect("init");
        ensure_secondary_index_reserved_region_initialized(0)
            .expect("ensure+init secondary region");
        let (_prop_off, _prop_len, sec_off, sec_len, _base) = current_regions_meta().expect("meta");
        assert!(sec_off > 0);
        assert!(sec_len >= ABP_STORE_HEADER_LEN + u64::from(ABP_PAGE_SIZE));
        with_state(|g| {
            let hdr = AbpStoreHeader::read_from(&g.mem, sec_off).expect("abp hdr");
            assert_eq!(hdr.page_size, ABP_PAGE_SIZE);
        });
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn native_ic_stable_memory_clones_share_backing_storage() {
        let mut mem1 = IcStableMemory::default();
        mem1.grow(32).expect("grow");
        let mut mem2 = mem1.clone();

        mem1.write(4, b"ABCD");
        let mut buf = [0u8; 4];
        mem2.read(4, &mut buf);
        assert_eq!(&buf, b"ABCD");

        mem2.write(12, b"WXYZ");
        let mut buf2 = [0u8; 4];
        mem1.read(12, &mut buf2);
        assert_eq!(&buf2, b"WXYZ");
    }

    #[test]
    fn reinit_clears_stale_reserved_regions_before_persist() {
        init_state(1024, 0).expect("init small graph");
        let old_tombstone_offset = with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            assert!(
                regions.vertex_tombstone_offset > 0,
                "small init allocates tombstone region"
            );
            regions.vertex_tombstone_offset
        });

        init_state(8192, 16_384).expect("reinit larger graph");

        with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            let pma_end = layout::total_memory_needed(
                g.num_vertices,
                g.elem_capacity,
                u64::from(g.segment_count),
            );
            assert!(
                regions.vertex_tombstone_offset >= pma_end,
                "fresh init must not retain a tombstone region below the new PMA end"
            );
            assert_ne!(
                regions.vertex_tombstone_offset, old_tombstone_offset,
                "fresh init should not reuse stale tombstone metadata from the previous graph"
            );
        });
    }

    #[test]
    fn expand_vertices_refreshes_secondary_index_extent_before_relocation() {
        use gleaph_types::Value;

        reset_metrics_and_quota_for_test();
        init_state(256, 0).expect("init");
        with_state_mut(|g| {
            g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
                .expect("create index");
        });
        ensure_secondary_index_reserved_region_initialized(0)
            .expect("ensure+init secondary region");
        persist_state_metadata().expect("persist and attach live secondary index");

        let (sec_off_before, sec_len_before) = with_state(|g| {
            assert!(
                g.has_live_eq_index(),
                "persist should attach live secondary index"
            );
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            (regions.secondary_index_offset, regions.secondary_index_len)
        });
        assert!(sec_off_before > 0, "secondary index region allocated");
        assert!(sec_len_before > 0, "secondary index region length recorded");

        with_state_mut(|g| {
            for vid in 0..g.vertex_count() as u32 {
                g.set_vertex_prop(vid, "uid".into(), Value::Int64(i64::from(vid) + 1))
                    .expect("set indexed prop");
            }
        });

        let stale_actual_len = with_state(|g| {
            let hdr = AbpStoreHeader::read_from(&g.mem, sec_off_before)
                .expect("secondary index header after live growth");
            let actual_len =
                ABP_STORE_HEADER_LEN + u64::from(hdr.next_page_id) * u64::from(hdr.page_size);
            assert!(
                actual_len > sec_len_before,
                "live ABP growth should exceed recorded metadata before PMA refresh"
            );
            actual_len
        });

        with_state_mut(|g| g.ensure_vertex(256).expect("expand vertices"));

        with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            let pma_end = layout::total_memory_needed(
                g.num_vertices,
                g.elem_capacity,
                u64::from(g.segment_count),
            );
            assert!(
                regions.secondary_index_offset >= pma_end,
                "secondary index must be relocated beyond the expanded PMA"
            );
            let hdr = AbpStoreHeader::read_from(&g.mem, regions.secondary_index_offset)
                .expect("secondary index header after relocation");
            let actual_len =
                ABP_STORE_HEADER_LEN + u64::from(hdr.next_page_id) * u64::from(hdr.page_size);
            assert_eq!(
                regions.secondary_index_len, actual_len,
                "relocation must use refreshed live ABP extent"
            );
            assert!(
                actual_len >= stale_actual_len,
                "relocated index should preserve all pages that existed before expansion"
            );

            let idx =
                AbpSecondaryEqIndex::from_memory(g.mem.clone(), regions.secondary_index_offset)
                    .expect("reopen relocated secondary index");
            let hits = idx
                .scan_vertices_eq("uid", &Value::Int64(256))
                .expect("scan relocated secondary index");
            assert_eq!(hits, vec![255]);
            assert!(
                !g.has_live_eq_index(),
                "PMA growth should invalidate the stale cached secondary-index handle"
            );
        });
    }

    #[test]
    fn expand_vertices_refreshes_property_store_extent_before_relocation() {
        use gleaph_types::Value;

        reset_metrics_and_quota_for_test();
        init_state(256, 0).expect("init");
        ensure_property_store_reserved_region_initialized(0)
            .expect("ensure+init property store region");

        let (prop_off_before, prop_len_before) = with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            (regions.property_store_offset, regions.property_store_len)
        });
        assert!(prop_off_before > 0, "property store region allocated");
        assert!(prop_len_before > 0, "property store region length recorded");

        with_state_mut(|g| {
            let mut store = AbpPropertyStore::from_memory(g.mem.clone(), prop_off_before)
                .expect("open property store");
            for vid in 0..g.vertex_count() as u32 {
                store
                    .set_vertex_prop(vid, "payload", Value::Text("x".repeat(256)))
                    .expect("grow property store");
            }
            g.mem = store.into_memory();
        });

        let stale_actual_len = with_state(|g| {
            let hdr = AbpStoreHeader::read_from(&g.mem, prop_off_before)
                .expect("property store header after live growth");
            let actual_len =
                ABP_STORE_HEADER_LEN + u64::from(hdr.next_page_id) * u64::from(hdr.page_size);
            assert!(
                actual_len > prop_len_before,
                "live property store growth should exceed recorded metadata before PMA refresh"
            );
            actual_len
        });

        with_state_mut(|g| g.ensure_vertex(256).expect("expand vertices"));

        with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            let pma_end = layout::total_memory_needed(
                g.num_vertices,
                g.elem_capacity,
                u64::from(g.segment_count),
            );
            assert!(
                regions.property_store_offset >= pma_end,
                "property store must be relocated beyond the expanded PMA"
            );
            let hdr = AbpStoreHeader::read_from(&g.mem, regions.property_store_offset)
                .expect("property store header after relocation");
            let actual_len =
                ABP_STORE_HEADER_LEN + u64::from(hdr.next_page_id) * u64::from(hdr.page_size);
            assert_eq!(
                regions.property_store_len, actual_len,
                "relocation must use refreshed property-store extent"
            );
            assert!(
                actual_len >= stale_actual_len,
                "relocated property store should preserve all pages that existed before expansion"
            );

            let store = AbpPropertyStore::from_memory(g.mem.clone(), regions.property_store_offset)
                .expect("reopen relocated property store");
            assert_eq!(
                store.get_vertex_prop(255, "payload"),
                Some(Value::Text("x".repeat(256)))
            );
        });
    }

    #[test]
    fn expand_vertices_refreshes_vertex_meta_extent_before_relocation() {
        reset_metrics_and_quota_for_test();
        init_state(256, 0).expect("init");
        persist_state_metadata().expect("persist and allocate vertex meta region");

        let (vertex_meta_off_before, vertex_meta_len_before) = with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            (regions.vertex_meta_offset, regions.vertex_meta_len)
        });
        assert!(vertex_meta_off_before > 0, "vertex meta region allocated");
        assert!(
            vertex_meta_len_before > 0,
            "vertex meta region length recorded"
        );

        with_state_mut(|g| {
            let mut table = VertexMetaTable::open(g.mem.clone(), vertex_meta_off_before)
                .expect("open vertex meta");
            for vid in 0..g.vertex_count() as u32 {
                table
                    .set_vertex_meta(
                        vid,
                        &gleaph_pma::VertexMeta {
                            labels: vec![format!("Label{vid}"), "Shared".into()],
                        },
                    )
                    .expect("grow vertex meta");
            }
            g.mem = table.into_memory();
        });

        let stale_actual_len = with_state(|g| {
            let hdr = AbpStoreHeader::read_from(&g.mem, vertex_meta_off_before)
                .expect("vertex meta header after live growth");
            let actual_len =
                ABP_STORE_HEADER_LEN + u64::from(hdr.next_page_id) * u64::from(hdr.page_size);
            assert!(
                actual_len > vertex_meta_len_before,
                "live vertex-meta growth should exceed recorded metadata before PMA refresh"
            );
            actual_len
        });

        with_state_mut(|g| g.ensure_vertex(256).expect("expand vertices"));

        with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            let pma_end = layout::total_memory_needed(
                g.num_vertices,
                g.elem_capacity,
                u64::from(g.segment_count),
            );
            assert!(
                regions.vertex_meta_offset >= pma_end,
                "vertex meta must be relocated beyond the expanded PMA"
            );
            let hdr = AbpStoreHeader::read_from(&g.mem, regions.vertex_meta_offset)
                .expect("vertex meta header after relocation");
            let actual_len =
                ABP_STORE_HEADER_LEN + u64::from(hdr.next_page_id) * u64::from(hdr.page_size);
            assert_eq!(
                regions.vertex_meta_len, actual_len,
                "relocation must use refreshed vertex-meta extent"
            );
            assert!(
                actual_len >= stale_actual_len,
                "relocated vertex meta should preserve all pages that existed before expansion"
            );

            let table = VertexMetaTable::open(g.mem.clone(), regions.vertex_meta_offset)
                .expect("reopen relocated vertex meta");
            let meta = table
                .get_vertex_meta(255)
                .expect("read relocated vertex meta");
            assert_eq!(
                meta.labels,
                vec!["Label255".to_string(), "Shared".to_string()]
            );
        });
    }

    #[test]
    fn fresh_init_header_version() {
        init_state(8, 0).expect("init");
        with_state(|g| {
            let h = layout::read_header(&g.mem);
            assert_eq!(
                h.version,
                gleaph_types::STABLE_VERSION,
                "fresh init should be V1"
            );
        });
    }

    #[test]
    fn runtime_snapshot_captures_runtime_inventory() {
        build_runtime_persistence_fixture();
        let expected = collect_runtime_inventory();

        persist_state_metadata().expect("persist");

        let snapshot = with_state(|g| {
            let meta = read_persist_meta(&g.mem)
                .expect("read meta")
                .expect("meta present");
            read_runtime_snapshot(&g.mem, &meta)
                .expect("read runtime snapshot")
                .expect("runtime snapshot present")
        });

        let mut snapshot_prepared: Vec<_> = snapshot
            .prepared_sources
            .into_iter()
            .map(|entry| (entry.name, entry.source, entry.options))
            .collect();
        snapshot_prepared.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(snapshot_prepared, expected.prepared_sources);
        assert_eq!(snapshot.certification, expected.certification);
    }

    #[test]
    fn stable_admin_config_blob_captures_admin_inventory() {
        build_runtime_persistence_fixture();
        persist_state_metadata().expect("persist");
        let expected = collect_runtime_inventory();

        let (regions, snapshot) = with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .expect("regions present");
            let snapshot = read_stable_admin_config_snapshot(&g.mem, &regions)
                .expect("read config blob")
                .expect("config blob present");
            (regions, snapshot)
        });

        assert!(regions.config_catalog_len > 0);

        let mut aliases = snapshot.graph_aliases;
        aliases.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(aliases, expected.aliases);
        assert_eq!(snapshot.registry_principal, expected.registry_principal);

        let mut acl_entries = snapshot.acl_entries;
        acl_entries.sort_by(|a, b| a.0.as_slice().cmp(b.0.as_slice()));
        assert_eq!(acl_entries, expected.acl_entries);

        let mut graph_types = snapshot.graph_types;
        graph_types.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(graph_types, expected.graph_types);
        assert_eq!(snapshot.active_graph_type, expected.active_graph_type);
        assert_eq!(snapshot.schemas, expected.schemas);
        assert_eq!(snapshot.strict_type_check, expected.strict_type_check);

        let mut constraints = snapshot.constraints;
        constraints.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(constraints, expected.constraints);
    }

    #[test]
    fn restore_state_from_memory_restores_runtime_inventory() {
        build_runtime_persistence_fixture();
        persist_state_metadata().expect("persist");
        let mem = with_state(|g| g.mem.clone());
        let expected = collect_runtime_inventory();

        clear_runtime_state_for_restore_test();
        restore_state_from_memory(mem, true).expect("restore from captured memory");

        assert_eq!(collect_runtime_inventory(), expected);
    }

    #[test]
    fn runtime_snapshot_replay_restores_behavior_on_existing_graph() {
        build_runtime_persistence_fixture();
        persist_state_metadata().expect("persist");
        let (snapshot, config) = with_state(|g| {
            let meta = read_persist_meta(&g.mem)
                .expect("read meta")
                .expect("meta present");
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .expect("regions present");
            (
                read_runtime_snapshot(&g.mem, &meta)
                    .expect("read runtime snapshot")
                    .expect("runtime snapshot present"),
                read_stable_admin_config_snapshot(&g.mem, &regions)
                    .expect("read config blob")
                    .expect("config blob present"),
            )
        });

        restore_runtime_state_from_snapshot(snapshot, config, true);

        assert_runtime_behavior_after_snapshot_replay();
    }

    #[test]
    fn quota_persists_across_restore_via_stable_metadata() {
        clear_runtime_state_for_restore_test();
        init_state(8, 0).expect("init");
        let expected_quota = UsageQuota {
            max_vertices: 123,
            max_edges: 456,
        };
        set_quota(expected_quota.clone());
        persist_state_metadata().expect("persist");

        let persisted_meta = with_state(|g| {
            read_persist_meta(&g.mem)
                .expect("read meta")
                .expect("meta present")
        });
        assert_eq!(persisted_meta.quota(), expected_quota);

        let mem = with_state(|g| g.mem.clone());
        clear_runtime_state_for_restore_test();
        restore_state_from_memory(mem, true).expect("restore");

        assert_eq!(get_quota(), expected_quota);
    }

    #[test]
    fn metrics_remain_volatile_across_restore() {
        clear_runtime_state_for_restore_test();
        init_state(8, 0).expect("init");
        increment_query_count();
        increment_mutation_count();
        increment_rejected_count();
        increment_algorithm_calls();
        persist_state_metadata().expect("persist");

        let mem = with_state(|g| g.mem.clone());
        clear_runtime_state_for_restore_test();
        restore_state_from_memory(mem, true).expect("restore");

        assert_eq!(
            with_metrics(|m| m.clone()),
            OperationalMetrics {
                query_count: 0,
                mutation_count: 0,
                rejected_count: 0,
                algorithm_calls: 0,
                stable_memory_bytes: 0,
            }
        );
    }

    #[test]
    fn acl_entries_survive_persist_restore_cycle() {
        reset_metrics_and_quota_for_test();
        init_state(4, 0).expect("init");

        let alice = Principal::from_text("2vxsx-fae").expect("alice");
        set_acl_entry(alice, AccessLevel::Write);

        persist_state_metadata().expect("persist");

        // Verify the config blob contains the ACL entry.
        let snapshot = with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .expect("regions present");
            read_stable_admin_config_snapshot(&g.mem, &regions)
                .expect("read config blob")
                .expect("config blob present")
        });
        assert_eq!(snapshot.acl_entries.len(), 1);
        assert_eq!(snapshot.acl_entries[0].0, alice);
        assert_eq!(snapshot.acl_entries[0].1, AccessLevel::Write);

        // Clear ACL in-memory and verify it's gone.
        ACL_MAP.with(|m| m.borrow_mut().clear());
        assert!(get_acl_entry(&alice).is_none());

        // Simulate restore by reading the snapshot and restoring ACLs.
        for (principal, level) in snapshot.acl_entries {
            set_acl_entry(principal, level);
        }
        assert_eq!(get_acl_entry(&alice), Some(AccessLevel::Write));
    }

    #[test]
    fn persist_restore_tombstones_via_bitset() {
        reset_metrics_and_quota_for_test();
        init_state(16, 0).expect("init");

        // Delete some vertices.
        with_state_mut(|g| {
            g.delete_vertex(3).expect("delete 3");
            g.delete_vertex(7).expect("delete 7");
            g.delete_vertex(15).expect("delete 15");
        });

        persist_state_metadata().expect("persist");

        // Verify the bitset region was allocated and written.
        with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            assert!(
                regions.vertex_tombstone_offset > 0,
                "tombstone region allocated"
            );
            assert!(regions.vertex_tombstone_len > 0, "tombstone region len > 0");

            let bs = VertexTombstoneBitset::open(
                g.mem.clone(),
                regions.vertex_tombstone_offset,
                g.num_vertices as u32,
            );
            assert!(bs.is_tombstoned(3));
            assert!(bs.is_tombstoned(7));
            assert!(bs.is_tombstoned(15));
            assert!(!bs.is_tombstoned(0));
            assert!(!bs.is_tombstoned(1));
        });
    }

    #[test]
    fn persist_restore_vertex_labels_via_abp() {
        reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init");

        // Add labels to some vertices.
        with_state_mut(|g| {
            g.add_vertex_label(0, "Person".to_string())
                .expect("label 0");
            g.add_vertex_label(0, "Admin".to_string())
                .expect("label 0 Admin");
            g.add_vertex_label(3, "Product".to_string())
                .expect("label 3");
        });

        persist_state_metadata().expect("persist");

        // Verify the VertexMetaTable region was allocated.
        with_state(|g| {
            let regions = read_regions_meta(&g.mem)
                .expect("read regions")
                .unwrap_or_default();
            assert!(
                regions.vertex_meta_offset > 0,
                "vertex_meta region allocated"
            );
            assert!(regions.vertex_meta_len > 0, "vertex_meta region len > 0");

            // Read directly from the ABP table.
            let tbl = VertexMetaTable::open(g.mem.clone(), regions.vertex_meta_offset)
                .expect("open vertex meta table");
            let meta0 = tbl.get_vertex_meta(0).expect("vertex 0 has labels");
            assert!(meta0.labels.contains(&"Person".to_string()));
            assert!(meta0.labels.contains(&"Admin".to_string()));
            let meta3 = tbl.get_vertex_meta(3).expect("vertex 3 has labels");
            assert_eq!(meta3.labels, vec!["Product"]);
            assert!(tbl.get_vertex_meta(1).is_none(), "vertex 1 has no labels");
        });
    }
}
