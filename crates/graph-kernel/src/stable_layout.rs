//! Stable-memory layout registry — descriptive mirror of canister `MemoryId` assignments.
//!
//! Runtime ids remain defined in each canister's `facade/stable/memory.rs`. This module is the
//! typed inventory for tests and documentation sync per [ADR 0007](design/adr/0007-stable-memory-layout.md).
//!
//! # Classification taxonomy
//!
//! Every stable `VirtualMemory` region is assigned exactly one [`StableMemoryClass`]. Classes follow
//! [stable-memory-inventory.md](design/storage/stable-memory-inventory.md) and the canonical/derived
//! split in the [refactoring roadmap](design/architecture/refactoring-roadmap.md).
//!
//! **Not in this registry:** heap-only regions classified as `ephemeral` in the inventory (graph
//! `PENDING` posting queues, router planner catalog, prepared plans). They have no `MemoryId`.
//!
//! # Functional audit (2026-06-12)
//!
//! All 68 stable regions were re-reviewed against runtime behavior. Class assignments match the
//! inventory; no reclassification was required. Notable distinctions captured in [`StableMemoryRegion::role`]:
//!
//! - **LARA reverse (15–29):** `Derived` because recovery is theoretical from forward CSR; hot path
//!   co-updates with forward. Associated free-span regions stay `Maintenance` (physical PMA bookkeeping).
//! - **Graph label telemetry (39–40):** `Telemetry` — event outbox toward router aggregates, not
//!   canonical graph membership (that is `VERTEX_LABEL_SETS` + index label postings).
//! - **`ROUTER_LABEL_STATS_PROJECTION`:** `Telemetry` — per-shard projection cursor (ADR 0015), not
//!   user mutation idempotency (`ROUTER_MUTATION_BY_CLIENT_KEY` is `Canonical`).

/// How a stable region relates to authoritative graph/router/index state.
///
/// See module-level docs for Gleaph-specific examples and audit notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StableMemoryClass {
    /// Authoritative facts required to interpret or recover the system without consulting
    /// derived indexes, telemetry aggregates, or maintenance bookkeeping.
    ///
    /// **Meaning:** If this region were empty, you could not truthfully answer domain questions
    /// about existence, values, placement, authorization, or mutation outcomes from other stable
    /// regions alone.
    ///
    /// **Gleaph examples:**
    /// - Forward LARA adjacency and payload bytes (`FWD_*` slabs, logs, blobs)
    /// - Vertex/edge property values, vertex label sets on the graph shard
    /// - Router shard registry, placement map, resolution catalogs (names ↔ ids)
    /// - Graph/router mutation idempotency records (`GRAPH_MUTATION_JOURNAL`,
    ///   `ROUTER_MUTATION_BY_CLIENT_KEY`)
    /// - Graph-index authorization and shard-owner maps
    ///
    /// **Not canonical:** reverse adjacency, postings, aliases, telemetry counts, free-span pools.
    ///
    /// **Query rule:** Canonical state wins when derived state disagrees
    /// ([derived-state-query-semantics.md](design/index/derived-state-query-semantics.md)).
    Canonical,

    /// Optimized or redundant state reconstructable from canonical stores, usually via a named
    /// rebuild, backfill, or (for LARA reverse) a full-graph replay.
    ///
    /// **Meaning:** The region may lag canonical state (async flush/backfill) or be kept in sync
    /// on every mutation (edge aliases, edge equality postings). Either way, it is not the
    /// authority for “does this vertex/edge/property exist?”
    ///
    /// **Gleaph examples:**
    /// - LARA reverse orientation (`REV_*` adjacency and payloads) — co-updated on DML, no public
    ///   scan-rebuild API
    /// - `EDGE_ALIASES` — sync rebuild API on graph shard
    /// - `INDEX_POSTINGS`, `INDEX_LABEL_POSTINGS` — graph-index projections; backfill from graph
    ///
    /// **Merge policy (ADR 0007):** Do not merge with canonical neighbor regions without benchmark
    /// proof and a layout ADR.
    Derived,

    /// Operational or physical bookkeeping that is neither domain data nor a query-facing index.
    ///
    /// **Meaning:** Supports compaction, deferred PMA work, free-span reuse, or admin repair
    /// cursors. Query hot paths must not depend on these stores
    /// ([lara.md](design/storage/lara.md) scan-path rule).
    ///
    /// **Gleaph examples:**
    /// - LARA `*_FREE_SPANS` / `*_FREE_SPAN_BY_START` pairs (forward and reverse)
    /// - `FWD_EDGE_SPAN_META`, `MAINTENANCE_QUEUE`, `DIRTY_WORK_ITEMS`
    /// - Router `ROUTER_*_BACKFILL_STATE` cursors — progress markers, not membership truth
    ///
    /// **Distinction from `Derived`:** Maintenance regions are not rebuilt to answer user queries;
    /// they are drained or advanced by internal/admin flows.
    Maintenance,

    /// Bidirectional stable maps between human-readable names and dense numeric ids, with shared
    /// allocation policy (`BidirectionalCatalog`).
    ///
    /// **Meaning:** Schema resolution for planning and wire encoding. Not vertex/edge property
    /// values and not per-vertex label membership.
    ///
    /// **Gleaph examples:** Router `ROUTER_*_LABEL_BY_NAME` / `BY_ID` pairs and
    /// `ROUTER_PROPERTY_BY_NAME` / `BY_ID`.
    ///
    /// **Owner:** Router only in the current layout. Graph stores property values by `PropertyId`
    /// without a local name catalog (ADR 0006).
    ///
    /// **Note:** Each catalog uses two `MemoryId` regions (name→id and id→name). Consolidation is
    /// a Phase 8 benchmark candidate (ADR 0007 P2).
    Catalog,

    /// Event-sourced or aggregated statistics derived from graph shard activity, plus the stable
    /// structures that move or deduplicate those events.
    ///
    /// **Meaning:** Specialized `Derived` state used for count-only queries and cross-shard label
    /// stats ([ADR 0004](design/adr/0004-label-index.md)). May lag canonical label membership on
    /// the graph shard when the outbox is not yet replayed.
    ///
    /// **Gleaph examples:**
    /// - Graph `LABEL_STATS_DELTA_SEQ` / `LABEL_STATS_DELTA_LOG` — per-shard delta source
    /// - Router `ROUTER_VERTEX_LABEL_STATS`, live-by-shard maps, `ROUTER_LABEL_STATS_PROJECTION`
    ///   (replay dedup)
    ///
    /// **Not telemetry:** `VERTEX_LABEL_SETS` (canonical membership), `INDEX_LABEL_POSTINGS`
    /// (derived postings for seeds/sieve).
    Telemetry,

    /// Legacy or transitional stable view retained for read compatibility while another store is
    /// authoritative for new writes.
    ///
    /// **Meaning:** Safe to retire when no stable snapshot depends on the region and benchmarks
    /// justify removal (ADR 0007 P1).
    ///
    /// **Gleaph:** No stable region currently uses this class (P1 `EDGE_WEIGHT_PROFILES` retired
    /// 2026-06-12; edge profiles are `EDGE_PAYLOAD_PROFILES` only).
    Compatibility,
}

impl StableMemoryClass {
    /// Lowercase name used in `design/storage/stable-memory-inventory.md`.
    pub const fn inventory_name(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::Derived => "derived",
            Self::Maintenance => "maintenance",
            Self::Catalog => "catalog",
            Self::Telemetry => "telemetry",
            Self::Compatibility => "compatibility",
        }
    }
}

/// One stable `VirtualMemory` region assigned via `MemoryManager`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StableMemoryRegion {
    pub symbol: &'static str,
    pub memory_id: u8,
    pub class: StableMemoryClass,
    pub owner_domain: &'static str,
    /// What this region stores at runtime (functional role, not classification).
    pub role: &'static str,
    /// Rebuild or backfill entry point when the region is derived or rebuildable.
    pub rebuild: Option<&'static str>,
}

/// Named layout for one canister.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StableCanisterLayout {
    pub canister: &'static str,
    pub regions: &'static [StableMemoryRegion],
}

impl StableCanisterLayout {
    pub const fn region_count(&self) -> usize {
        self.regions.len()
    }

    pub const fn max_memory_id(&self) -> Option<u8> {
        if self.regions.is_empty() {
            return None;
        }
        let mut i = 1usize;
        let mut max = self.regions[0].memory_id;
        while i < self.regions.len() {
            let id = self.regions[i].memory_id;
            if id > max {
                max = id;
            }
            i += 1;
        }
        Some(max)
    }
}

const fn region(
    symbol: &'static str,
    memory_id: u8,
    class: StableMemoryClass,
    owner_domain: &'static str,
    role: &'static str,
    rebuild: Option<&'static str>,
) -> StableMemoryRegion {
    StableMemoryRegion {
        symbol,
        memory_id,
        class,
        owner_domain,
        role,
        rebuild,
    }
}

/// Graph canister — LARA bundle (0–31) + facade (32–40). Baseline: ADR 0007 §2 / ADR 0008.
pub static GRAPH_STABLE_LAYOUT: StableCanisterLayout = StableCanisterLayout {
    canister: "graph",
    regions: &[
        // Forward orientation — canonical adjacency
        region(
            "FWD_VERTICES",
            0,
            StableMemoryClass::Canonical,
            "lara/adjacency",
            "Per-vertex row metadata for forward CSR",
            None,
        ),
        region(
            "FWD_BUCKETS",
            1,
            StableMemoryClass::Canonical,
            "lara/adjacency",
            "Per-vertex labeled edge bucket roots",
            None,
        ),
        region(
            "FWD_BUCKET_FREE_SPANS",
            2,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Retired forward bucket physical byte ranges",
            None,
        ),
        region(
            "FWD_BUCKET_FREE_SPAN_BY_START",
            3,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Index into forward bucket free-span store",
            None,
        ),
        region(
            "FWD_EDGE_COUNTS",
            4,
            StableMemoryClass::Canonical,
            "lara/adjacency",
            "Per-vertex forward edge counts by label",
            None,
        ),
        region(
            "FWD_EDGES",
            5,
            StableMemoryClass::Canonical,
            "lara/adjacency",
            "Forward edge slab (CSR edge rows)",
            None,
        ),
        region(
            "FWD_EDGE_LOG",
            6,
            StableMemoryClass::Canonical,
            "lara/adjacency",
            "Append log for forward edge value updates",
            None,
        ),
        region(
            "FWD_EDGE_SPAN_META",
            7,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Per-vertex forward edge span compaction metadata",
            None,
        ),
        region(
            "FWD_EDGE_FREE_SPANS",
            8,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Retired forward edge physical byte ranges",
            None,
        ),
        region(
            "FWD_EDGE_FREE_SPAN_BY_START",
            9,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Index into forward edge free-span store",
            None,
        ),
        // Forward payload — canonical bytes
        region(
            "FWD_PAYLOAD_SLAB",
            10,
            StableMemoryClass::Canonical,
            "lara/payload",
            "Dense labeled edge payload bytes",
            None,
        ),
        region(
            "FWD_PAYLOAD_FREE_SPANS",
            11,
            StableMemoryClass::Maintenance,
            "lara/payload",
            "Retired forward payload physical ranges",
            None,
        ),
        region(
            "FWD_PAYLOAD_FREE_SPAN_BY_START",
            12,
            StableMemoryClass::Maintenance,
            "lara/payload",
            "Index into forward payload free-span store",
            None,
        ),
        region(
            "FWD_PAYLOAD_LOG",
            13,
            StableMemoryClass::Canonical,
            "lara/payload",
            "Value log for inline payload updates",
            None,
        ),
        region(
            "FWD_PAYLOAD_BLOBS",
            14,
            StableMemoryClass::Canonical,
            "lara/payload",
            "Out-of-line large payload blobs",
            None,
        ),
        // Reverse orientation — derived adjacency (sync co-update)
        region(
            "REV_VERTICES",
            15,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Reverse CSR vertex rows (mirror of forward)",
            None,
        ),
        region(
            "REV_BUCKETS",
            16,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Reverse labeled edge bucket roots",
            None,
        ),
        region(
            "REV_BUCKET_FREE_SPANS",
            17,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Retired reverse bucket physical ranges",
            None,
        ),
        region(
            "REV_BUCKET_FREE_SPAN_BY_START",
            18,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Index into reverse bucket free-span store",
            None,
        ),
        region(
            "REV_EDGE_COUNTS",
            19,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Per-vertex reverse edge counts",
            None,
        ),
        region(
            "REV_EDGES",
            20,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Reverse edge slab",
            None,
        ),
        region(
            "REV_EDGE_LOG",
            21,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Reverse edge value log",
            None,
        ),
        region(
            "REV_EDGE_SPAN_META",
            22,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Reverse edge span compaction metadata",
            None,
        ),
        region(
            "REV_EDGE_FREE_SPANS",
            23,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Retired reverse edge physical ranges",
            None,
        ),
        region(
            "REV_EDGE_FREE_SPAN_BY_START",
            24,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Index into reverse edge free-span store",
            None,
        ),
        region(
            "REV_PAYLOAD_SLAB",
            25,
            StableMemoryClass::Derived,
            "lara/payload",
            "Reverse payload slab",
            None,
        ),
        region(
            "REV_PAYLOAD_FREE_SPANS",
            26,
            StableMemoryClass::Maintenance,
            "lara/payload",
            "Retired reverse payload physical ranges",
            None,
        ),
        region(
            "REV_PAYLOAD_FREE_SPAN_BY_START",
            27,
            StableMemoryClass::Maintenance,
            "lara/payload",
            "Index into reverse payload free-span store",
            None,
        ),
        region(
            "REV_PAYLOAD_LOG",
            28,
            StableMemoryClass::Derived,
            "lara/payload",
            "Reverse payload value log",
            None,
        ),
        region(
            "REV_PAYLOAD_BLOBS",
            29,
            StableMemoryClass::Derived,
            "lara/payload",
            "Reverse out-of-line payload blobs",
            None,
        ),
        // LARA deferred maintenance
        region(
            "MAINTENANCE_QUEUE",
            30,
            StableMemoryClass::Maintenance,
            "lara/maintenance",
            "Deferred PMA maintenance work queue",
            None,
        ),
        region(
            "DIRTY_WORK_ITEMS",
            31,
            StableMemoryClass::Maintenance,
            "lara/maintenance",
            "Dirty maintenance work tracking",
            None,
        ),
        // Graph facade
        region(
            "VERTEX_LABEL_SETS",
            32,
            StableMemoryClass::Canonical,
            "labels",
            "Per-vertex label id sets (membership, not name catalog)",
            None,
        ),
        region(
            "VERTEX_PROPERTIES",
            33,
            StableMemoryClass::Canonical,
            "properties",
            "Vertex property values keyed by PropertyId",
            None,
        ),
        region(
            "EDGE_PROPERTIES",
            34,
            StableMemoryClass::Canonical,
            "properties",
            "Edge property values keyed by canonical edge identity",
            None,
        ),
        region(
            "EDGE_ALIASES",
            35,
            StableMemoryClass::Derived,
            "adjacency",
            "Undirected/reverse expand alias index",
            Some("rebuild_edge_aliases"),
        ),
        region(
            "GRAPH_METADATA",
            36,
            StableMemoryClass::Canonical,
            "federation metadata",
            "Shard id, router principal, index principal",
            None,
        ),
        region(
            "LABEL_STATS_DELTA_SEQ",
            37,
            StableMemoryClass::Telemetry,
            "label stats projection",
            "Monotonic seq for label stats delta log",
            None,
        ),
        region(
            "LABEL_STATS_DELTA_LOG",
            38,
            StableMemoryClass::Telemetry,
            "label stats projection",
            "Stable log of LabelStatsDelta events for router projection",
            None,
        ),
        region(
            "GRAPH_MUTATION_JOURNAL",
            39,
            StableMemoryClass::Canonical,
            "idempotency",
            "Graph mutation journal (outcome + emitted delta seq range)",
            None,
        ),
    ],
};

/// Router canister — registry, placement, catalogs, telemetry, backfill. Baseline: ADR 0007 §2.
pub static ROUTER_STABLE_LAYOUT: StableCanisterLayout = StableCanisterLayout {
    canister: "router",
    regions: &[
        region(
            "ROUTER_CONTROLLERS",
            0,
            StableMemoryClass::Canonical,
            "auth",
            "Controller principal allowlist",
            None,
        ),
        region(
            "ROUTER_GRAPHS",
            1,
            StableMemoryClass::Canonical,
            "registry",
            "GraphId → registry entry",
            None,
        ),
        region(
            "ROUTER_SHARDS",
            2,
            StableMemoryClass::Canonical,
            "registry",
            "ShardId → shard registry entry",
            None,
        ),
        region(
            "ROUTER_SHARD_BY_GRAPH",
            3,
            StableMemoryClass::Canonical,
            "registry",
            "Graph canister principal → ShardId",
            None,
        ),
        region(
            "ROUTER_PLACEMENTS",
            4,
            StableMemoryClass::Canonical,
            "placement",
            "GlobalVertexId → physical placement record",
            None,
        ),
        region(
            "ROUTER_VERTEX_LABEL_BY_NAME",
            5,
            StableMemoryClass::Catalog,
            "resolution",
            "Vertex label name → VertexLabelId",
            None,
        ),
        region(
            "ROUTER_VERTEX_LABEL_BY_ID",
            6,
            StableMemoryClass::Catalog,
            "resolution",
            "VertexLabelId → label name",
            None,
        ),
        region(
            "ROUTER_EDGE_LABEL_BY_NAME",
            7,
            StableMemoryClass::Catalog,
            "resolution",
            "Edge label name → EdgeLabelId",
            None,
        ),
        region(
            "ROUTER_EDGE_LABEL_BY_ID",
            8,
            StableMemoryClass::Catalog,
            "resolution",
            "EdgeLabelId → label name",
            None,
        ),
        region(
            "ROUTER_PROPERTY_BY_NAME",
            9,
            StableMemoryClass::Catalog,
            "resolution",
            "Property name → PropertyId",
            None,
        ),
        region(
            "ROUTER_PROPERTY_BY_ID",
            10,
            StableMemoryClass::Catalog,
            "resolution",
            "PropertyId → property name",
            None,
        ),
        region(
            "ROUTER_AUTH_PRINCIPAL_RECORDS",
            11,
            StableMemoryClass::Canonical,
            "auth",
            "Per-principal RBAC / prepared-query auth records",
            None,
        ),
        region(
            "ROUTER_VERTEX_LABEL_STATS",
            12,
            StableMemoryClass::Telemetry,
            "label telemetry",
            "Aggregated vertex label usage stats",
            Some("admin_label_stats_projection_step"),
        ),
        region(
            "ROUTER_EDGE_LABEL_STATS",
            13,
            StableMemoryClass::Telemetry,
            "label telemetry",
            "Aggregated edge label usage stats",
            Some("admin_label_stats_projection_step"),
        ),
        region(
            "ROUTER_VERTEX_LABEL_LIVE_BY_SHARD",
            14,
            StableMemoryClass::Telemetry,
            "label telemetry",
            "Per-shard live vertex counts per label",
            Some("admin_label_stats_projection_step"),
        ),
        region(
            "ROUTER_EDGE_LABEL_LIVE_BY_SHARD",
            15,
            StableMemoryClass::Telemetry,
            "label telemetry",
            "Per-shard live edge counts per label",
            Some("admin_label_stats_projection_step"),
        ),
        region(
            "ROUTER_MUTATION_COUNTER",
            16,
            StableMemoryClass::Canonical,
            "idempotency",
            "Monotonic router mutation id allocator",
            None,
        ),
        region(
            "ROUTER_LABEL_STATS_PROJECTION",
            17,
            StableMemoryClass::Telemetry,
            "label stats projection",
            "ShardId → applied_through_seq for graph label stats deltas (ADR 0015)",
            None,
        ),
        region(
            "ROUTER_MUTATION_BY_CLIENT_KEY",
            18,
            StableMemoryClass::Canonical,
            "idempotency",
            "Client mutation key → router mutation record",
            None,
        ),
        region(
            "ROUTER_LABEL_BACKFILL_STATE",
            19,
            StableMemoryClass::Maintenance,
            "label backfill",
            "Per-shard cursor for label posting backfill admin",
            None,
        ),
        region(
            "ROUTER_PROPERTY_BACKFILL_STATE",
            20,
            StableMemoryClass::Maintenance,
            "property backfill",
            "Per-shard cursor for property posting backfill admin",
            None,
        ),
        region(
            "ROUTER_EDGE_PAYLOAD_PROFILES",
            21,
            StableMemoryClass::Catalog,
            "edge payload schema",
            "EdgeLabelId → EdgePayloadProfile (ADR 0008 SSOT)",
            None,
        ),
        region(
            "ROUTER_NAMED_INDEXES",
            22,
            StableMemoryClass::Catalog,
            "index planner catalog",
            "(graph_id, index_name_id) → IndexDefRecord (ADR 0009 DDL metadata)",
            None,
        ),
        region(
            "ROUTER_INDEXED_PROPERTY_SET",
            23,
            StableMemoryClass::Catalog,
            "index planner catalog",
            "(graph_id, kind, property_id) membership for planner + shard fan-out",
            None,
        ),
        region(
            "ROUTER_GRAPH_BY_NAME",
            24,
            StableMemoryClass::Catalog,
            "resolution",
            "Graph name → GraphId (ADR 0011)",
            None,
        ),
        region(
            "ROUTER_GRAPH_BY_ID",
            25,
            StableMemoryClass::Catalog,
            "resolution",
            "GraphId → graph name (ADR 0011)",
            None,
        ),
        region(
            "ROUTER_INDEX_NAME_BY_NAME",
            26,
            StableMemoryClass::Catalog,
            "resolution",
            "Graph-scoped index name → IndexNameId (ADR 0011)",
            None,
        ),
        region(
            "ROUTER_INDEX_NAME_BY_ID",
            27,
            StableMemoryClass::Catalog,
            "resolution",
            "Graph-scoped IndexNameId → index name (ADR 0011)",
            None,
        ),
        region(
            "ROUTER_SHARDS_BY_GRAPH_ID",
            28,
            StableMemoryClass::Canonical,
            "registry",
            "GraphId → shard id list (ADR 0011)",
            None,
        ),
        region(
            "ROUTER_PREPARED_PLANS",
            29,
            StableMemoryClass::Canonical,
            "prepared queries",
            "PreparedPlanKey → versioned plan wire blob",
            None,
        ),
        region(
            "ROUTER_GRAPH_TYPE_DEFINITIONS",
            30,
            StableMemoryClass::Catalog,
            "graph type catalog",
            "GraphTypeId → graph type definition (ADR 0014)",
            None,
        ),
        region(
            "ROUTER_GRAPH_SCHEMA_BINDINGS",
            31,
            StableMemoryClass::Catalog,
            "graph type catalog",
            "GraphId → property graph schema binding (ADR 0013)",
            None,
        ),
        region(
            "ROUTER_GRAPH_TYPE_BY_NAME",
            32,
            StableMemoryClass::Catalog,
            "graph type name catalog",
            "Graph type name → GraphTypeId (ADR 0014)",
            None,
        ),
        region(
            "ROUTER_GRAPH_TYPE_BY_ID",
            33,
            StableMemoryClass::Catalog,
            "graph type name catalog",
            "GraphTypeId → graph type name (ADR 0014)",
            None,
        ),
    ],
};

/// Graph-index canister. Baseline: ADR 0007 §2.
pub static INDEX_STABLE_LAYOUT: StableCanisterLayout = StableCanisterLayout {
    canister: "graph-index",
    regions: &[
        region(
            "INDEX_ADMINS",
            0,
            StableMemoryClass::Canonical,
            "authorization",
            "Index canister admin principal set",
            None,
        ),
        region(
            "INDEX_SHARD_OWNERS",
            1,
            StableMemoryClass::Canonical,
            "shard ownership",
            "ShardId → owning graph canister principal",
            None,
        ),
        region(
            "INDEX_POSTINGS",
            2,
            StableMemoryClass::Derived,
            "property postings",
            "Global property equality/range posting set",
            Some("backfill_property_postings"),
        ),
        region(
            "INDEX_ROUTER",
            3,
            StableMemoryClass::Canonical,
            "router authorization",
            "Authorized router canister principal cell",
            None,
        ),
        region(
            "INDEX_LABEL_POSTINGS",
            4,
            StableMemoryClass::Derived,
            "label postings",
            "Global vertex label membership posting set",
            Some("backfill_label_postings"),
        ),
        region(
            "INDEX_EDGE_POSTINGS",
            5,
            StableMemoryClass::Derived,
            "edge property postings",
            "Global edge property equality posting set (ADR 0009)",
            Some("backfill_edge_property_postings"),
        ),
    ],
};

/// Validates consecutive ids starting at zero and unique symbols.
pub fn validate_layout(layout: &StableCanisterLayout) -> Result<(), LayoutValidationError> {
    if layout.regions.is_empty() {
        return Err(LayoutValidationError::Empty {
            canister: layout.canister,
        });
    }

    let mut i = 0usize;
    while i < layout.regions.len() {
        let expected_id = u8::try_from(i).map_err(|_| LayoutValidationError::IdOverflow {
            canister: layout.canister,
            index: i,
        })?;
        let region = &layout.regions[i];
        if region.memory_id != expected_id {
            return Err(LayoutValidationError::NonConsecutiveId {
                canister: layout.canister,
                symbol: region.symbol,
                memory_id: region.memory_id,
                expected: expected_id,
            });
        }

        let mut j = i + 1;
        while j < layout.regions.len() {
            if layout.regions[j].symbol == region.symbol {
                return Err(LayoutValidationError::DuplicateSymbol {
                    canister: layout.canister,
                    symbol: region.symbol,
                });
            }
            j += 1;
        }
        i += 1;
    }

    Ok(())
}

/// Layout registry validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutValidationError {
    Empty {
        canister: &'static str,
    },
    IdOverflow {
        canister: &'static str,
        index: usize,
    },
    NonConsecutiveId {
        canister: &'static str,
        symbol: &'static str,
        memory_id: u8,
        expected: u8,
    },
    DuplicateSymbol {
        canister: &'static str,
        symbol: &'static str,
    },
}

/// Class invariants enforced by the functional audit.
pub fn validate_class_invariants(layout: &StableCanisterLayout) -> Result<(), ClassInvariantError> {
    let mut i = 0usize;
    while i < layout.regions.len() {
        let region = &layout.regions[i];
        match region.class {
            StableMemoryClass::Compatibility => {
                if region.rebuild.is_some() {
                    return Err(ClassInvariantError::CompatibilityWithRebuild {
                        canister: layout.canister,
                        symbol: region.symbol,
                    });
                }
            }
            StableMemoryClass::Maintenance | StableMemoryClass::Catalog => {
                if region.rebuild.is_some() {
                    return Err(ClassInvariantError::UnexpectedRebuild {
                        canister: layout.canister,
                        symbol: region.symbol,
                        class: region.class,
                    });
                }
            }
            StableMemoryClass::Derived => {
                // LARA reverse regions have no rebuild API; graph/router derived indexes must name one.
                if region.rebuild.is_none()
                    && !region.symbol.starts_with("REV_")
                    && !region.symbol.starts_with("FWD_")
                {
                    // INDEX_* and EDGE_* derived stores require rebuild/backfill names.
                    if region.symbol.starts_with("INDEX_") || region.symbol == "EDGE_ALIASES" {
                        return Err(ClassInvariantError::DerivedWithoutRebuild {
                            canister: layout.canister,
                            symbol: region.symbol,
                        });
                    }
                }
            }
            StableMemoryClass::Canonical | StableMemoryClass::Telemetry => {}
        }
        i += 1;
    }
    Ok(())
}

/// Class invariant violation detected during registry audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassInvariantError {
    CompatibilityWithRebuild {
        canister: &'static str,
        symbol: &'static str,
    },
    UnexpectedRebuild {
        canister: &'static str,
        symbol: &'static str,
        class: StableMemoryClass,
    },
    DerivedWithoutRebuild {
        canister: &'static str,
        symbol: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_layout(layout: &StableCanisterLayout) {
        validate_layout(layout).expect("layout ids");
        validate_class_invariants(layout).expect("class invariants");
    }

    #[test]
    fn graph_layout_registry_matches_baseline() {
        assert_layout(&GRAPH_STABLE_LAYOUT);
        assert_eq!(GRAPH_STABLE_LAYOUT.region_count(), 40);
        assert_eq!(GRAPH_STABLE_LAYOUT.max_memory_id(), Some(39));
        assert_eq!(GRAPH_STABLE_LAYOUT.regions[0].symbol, "FWD_VERTICES");
        assert_eq!(
            GRAPH_STABLE_LAYOUT.regions[39].symbol,
            "GRAPH_MUTATION_JOURNAL"
        );
        assert_eq!(
            GRAPH_STABLE_LAYOUT.regions[35].class,
            StableMemoryClass::Derived
        );
    }

    #[test]
    fn router_layout_registry_matches_baseline() {
        assert_layout(&ROUTER_STABLE_LAYOUT);
        assert_eq!(ROUTER_STABLE_LAYOUT.region_count(), 34);
        assert_eq!(ROUTER_STABLE_LAYOUT.max_memory_id(), Some(33));
        assert_eq!(
            ROUTER_STABLE_LAYOUT.regions[17].class,
            StableMemoryClass::Telemetry
        );
        assert_eq!(
            ROUTER_STABLE_LAYOUT.regions[17].symbol,
            "ROUTER_LABEL_STATS_PROJECTION"
        );
    }

    #[test]
    fn index_layout_registry_matches_baseline() {
        assert_layout(&INDEX_STABLE_LAYOUT);
        assert_eq!(INDEX_STABLE_LAYOUT.region_count(), 6);
        assert_eq!(INDEX_STABLE_LAYOUT.max_memory_id(), Some(5));
    }

    #[test]
    fn stable_memory_class_inventory_names() {
        assert_eq!(StableMemoryClass::Canonical.inventory_name(), "canonical");
        assert_eq!(StableMemoryClass::Telemetry.inventory_name(), "telemetry");
    }
}
