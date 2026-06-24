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
//! - **`ROUTER_LABEL_STATS_PROJECTION`:** `Telemetry` — per-`GraphShardKey` projection cursor (ADR 0015/0019), not
//!   user mutation idempotency (`ROUTER_MUTATION_BY_CLIENT_KEY` is `Canonical`).
//!
//! ## Correction (2026-06-18)
//!
//! `ROUTER_SHARD_BY_GRAPH` and `ROUTER_SHARDS_BY_GRAPH_ID` were reclassified `Canonical` → `Derived`
//! to match `stable-memory-inventory.md` (router registry section). `ROUTER_SHARDS` is the shard
//! dispatch source of truth; both lookup regions are denormalized projections kept consistent at the
//! registry commit boundary (`commit_register_shard` / `check_registry_invariants`).

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
    /// - LARA reverse orientation (`REV_*` adjacency and payloads) — co-updated on DML, plus a
    ///   differential `rebuild_reverse_adjacency` repair API
    /// - `EDGE_ALIASES` — sync rebuild API on graph shard
    /// - `INDEX_VERTEX_POSTINGS`, `INDEX_VERTEX_LABEL_POSTINGS` — graph-index projections; backfill from graph
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
    /// **Not telemetry:** `VERTEX_LABEL_SETS` (canonical membership), `INDEX_VERTEX_LABEL_POSTINGS`
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

/// How a region's contents can be reconstructed — or audited for drift — when needed.
///
/// Replaces an earlier `Option<&'static str>`. There, `None` was overloaded between "this region
/// has no rebuild concept" and "derived, but kept consistent inline with no rebuild API", and the
/// class invariants could only catch the difference for `INDEX_*` / `EDGE_ALIASES`. Making the
/// sync-co-update case explicit lets [`validate_class_invariants`] forbid an unspecified `None` on
/// any `Derived` region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildPath {
    /// No rebuild path. The region is authoritative (`Canonical`), physical/admin bookkeeping
    /// (`Maintenance`), a resolution catalog (`Catalog`), a compatibility view, or a telemetry
    /// event source/cursor — nothing reconstructs it from other stable state.
    None,
    /// Derived state kept consistent with its canonical source inside the same mutation / commit
    /// boundary; there is no standalone scan-and-rebuild API.
    ///
    /// **Gleaph:** the router registry projections denormalized from `ROUTER_SHARDS`
    /// (`ROUTER_SHARD_BY_GRAPH`, `ROUTER_SHARDS_BY_GRAPH_ID`, commit-synced). The LARA reverse
    /// orientation (`REV_*`) is also co-updated on DML, but it additionally exposes a standalone
    /// differential repair (`rebuild_reverse_adjacency`), so it is classified `Named` rather than
    /// `SyncCoUpdate`.
    SyncCoUpdate,
    /// Rebuildable or backfillable from canonical state via a named entry point
    /// (e.g. `rebuild_edge_aliases`, `backfill_label_postings`, `admin_label_stats_projection_step`).
    Named(&'static str),
}

impl RebuildPath {
    /// The named rebuild/backfill entry point, if this region declares one.
    pub const fn named(self) -> Option<&'static str> {
        match self {
            Self::Named(name) => Some(name),
            Self::None | Self::SyncCoUpdate => None,
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
    /// How this region is reconstructed or kept consistent. See [`RebuildPath`].
    pub rebuild: RebuildPath,
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
    rebuild: RebuildPath,
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

/// Graph canister — LARA bundle (0–31) + facade (32–39), 40 regions. Baseline: ADR 0007 §2 / ADR 0008.
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
            RebuildPath::None,
        ),
        region(
            "FWD_BUCKETS",
            1,
            StableMemoryClass::Canonical,
            "lara/adjacency",
            "Per-vertex labeled edge bucket roots",
            RebuildPath::None,
        ),
        region(
            "FWD_BUCKET_FREE_SPANS",
            2,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Retired forward bucket physical byte ranges",
            RebuildPath::None,
        ),
        region(
            "FWD_BUCKET_FREE_SPAN_BY_START",
            3,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Index into forward bucket free-span store",
            RebuildPath::None,
        ),
        region(
            "FWD_EDGE_COUNTS",
            4,
            StableMemoryClass::Canonical,
            "lara/adjacency",
            "Per-vertex forward edge counts by label",
            RebuildPath::None,
        ),
        region(
            "FWD_EDGES",
            5,
            StableMemoryClass::Canonical,
            "lara/adjacency",
            "Forward edge slab (CSR edge rows)",
            RebuildPath::None,
        ),
        region(
            "FWD_EDGE_LOG",
            6,
            StableMemoryClass::Canonical,
            "lara/adjacency",
            "Append log for forward edge value updates",
            RebuildPath::None,
        ),
        region(
            "FWD_EDGE_SPAN_META",
            7,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Per-vertex forward edge span compaction metadata",
            RebuildPath::None,
        ),
        region(
            "FWD_EDGE_FREE_SPANS",
            8,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Retired forward edge physical byte ranges",
            RebuildPath::None,
        ),
        region(
            "FWD_EDGE_FREE_SPAN_BY_START",
            9,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Index into forward edge free-span store",
            RebuildPath::None,
        ),
        // Forward payload — canonical bytes
        region(
            "FWD_PAYLOAD_SLAB",
            10,
            StableMemoryClass::Canonical,
            "lara/payload",
            "Dense labeled edge payload bytes",
            RebuildPath::None,
        ),
        region(
            "FWD_PAYLOAD_FREE_SPANS",
            11,
            StableMemoryClass::Maintenance,
            "lara/payload",
            "Retired forward payload physical ranges",
            RebuildPath::None,
        ),
        region(
            "FWD_PAYLOAD_FREE_SPAN_BY_START",
            12,
            StableMemoryClass::Maintenance,
            "lara/payload",
            "Index into forward payload free-span store",
            RebuildPath::None,
        ),
        region(
            "FWD_PAYLOAD_LOG",
            13,
            StableMemoryClass::Canonical,
            "lara/payload",
            "Value log for inline payload updates",
            RebuildPath::None,
        ),
        region(
            "FWD_PAYLOAD_BLOBS",
            14,
            StableMemoryClass::Canonical,
            "lara/payload",
            "Out-of-line large payload blobs",
            RebuildPath::None,
        ),
        // Reverse orientation — derived adjacency (co-updated on DML; differential repair API)
        region(
            "REV_VERTICES",
            15,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Reverse CSR vertex rows (mirror of forward)",
            RebuildPath::Named("rebuild_reverse_adjacency"),
        ),
        region(
            "REV_BUCKETS",
            16,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Reverse labeled edge bucket roots",
            RebuildPath::Named("rebuild_reverse_adjacency"),
        ),
        region(
            "REV_BUCKET_FREE_SPANS",
            17,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Retired reverse bucket physical ranges",
            RebuildPath::None,
        ),
        region(
            "REV_BUCKET_FREE_SPAN_BY_START",
            18,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Index into reverse bucket free-span store",
            RebuildPath::None,
        ),
        region(
            "REV_EDGE_COUNTS",
            19,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Per-vertex reverse edge counts",
            RebuildPath::Named("rebuild_reverse_adjacency"),
        ),
        region(
            "REV_EDGES",
            20,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Reverse edge slab",
            RebuildPath::Named("rebuild_reverse_adjacency"),
        ),
        region(
            "REV_EDGE_LOG",
            21,
            StableMemoryClass::Derived,
            "lara/adjacency",
            "Reverse edge value log",
            RebuildPath::Named("rebuild_reverse_adjacency"),
        ),
        region(
            "REV_EDGE_SPAN_META",
            22,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Reverse edge span compaction metadata",
            RebuildPath::None,
        ),
        region(
            "REV_EDGE_FREE_SPANS",
            23,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Retired reverse edge physical ranges",
            RebuildPath::None,
        ),
        region(
            "REV_EDGE_FREE_SPAN_BY_START",
            24,
            StableMemoryClass::Maintenance,
            "lara/adjacency",
            "Index into reverse edge free-span store",
            RebuildPath::None,
        ),
        region(
            "REV_PAYLOAD_SLAB",
            25,
            StableMemoryClass::Derived,
            "lara/payload",
            "Reverse payload slab",
            RebuildPath::Named("rebuild_reverse_adjacency"),
        ),
        region(
            "REV_PAYLOAD_FREE_SPANS",
            26,
            StableMemoryClass::Maintenance,
            "lara/payload",
            "Retired reverse payload physical ranges",
            RebuildPath::None,
        ),
        region(
            "REV_PAYLOAD_FREE_SPAN_BY_START",
            27,
            StableMemoryClass::Maintenance,
            "lara/payload",
            "Index into reverse payload free-span store",
            RebuildPath::None,
        ),
        region(
            "REV_PAYLOAD_LOG",
            28,
            StableMemoryClass::Derived,
            "lara/payload",
            "Reverse payload value log",
            RebuildPath::Named("rebuild_reverse_adjacency"),
        ),
        region(
            "REV_PAYLOAD_BLOBS",
            29,
            StableMemoryClass::Derived,
            "lara/payload",
            "Reverse out-of-line payload blobs",
            RebuildPath::Named("rebuild_reverse_adjacency"),
        ),
        // LARA deferred maintenance
        region(
            "MAINTENANCE_QUEUE",
            30,
            StableMemoryClass::Maintenance,
            "lara/maintenance",
            "Deferred PMA maintenance work queue",
            RebuildPath::None,
        ),
        region(
            "DIRTY_WORK_ITEMS",
            31,
            StableMemoryClass::Maintenance,
            "lara/maintenance",
            "Dirty maintenance work tracking",
            RebuildPath::None,
        ),
        // Graph facade
        region(
            "VERTEX_LABEL_SETS",
            32,
            StableMemoryClass::Canonical,
            "labels",
            "Per-vertex label id sets (membership, not name catalog)",
            RebuildPath::None,
        ),
        region(
            "VERTEX_PROPERTIES",
            33,
            StableMemoryClass::Canonical,
            "properties",
            "Vertex property values keyed by PropertyId",
            RebuildPath::None,
        ),
        region(
            "EDGE_PROPERTIES",
            34,
            StableMemoryClass::Canonical,
            "properties",
            "Edge property values keyed by canonical edge identity",
            RebuildPath::None,
        ),
        region(
            "EDGE_ALIASES",
            35,
            StableMemoryClass::Derived,
            "adjacency",
            "Undirected/reverse expand alias index",
            RebuildPath::Named("rebuild_edge_aliases"),
        ),
        region(
            "GRAPH_METADATA",
            36,
            StableMemoryClass::Canonical,
            "federation metadata",
            "Shard id, router principal, index principal",
            RebuildPath::None,
        ),
        region(
            "LABEL_STATS_DELTA_SEQ",
            37,
            StableMemoryClass::Telemetry,
            "label stats projection",
            "Monotonic seq for label stats delta log",
            RebuildPath::None,
        ),
        region(
            "LABEL_STATS_DELTA_LOG",
            38,
            StableMemoryClass::Telemetry,
            "label stats projection",
            "Stable log of LabelStatsDelta events for router projection",
            RebuildPath::None,
        ),
        region(
            "GRAPH_MUTATION_JOURNAL",
            39,
            StableMemoryClass::Canonical,
            "idempotency",
            "Graph mutation journal (outcome + emitted delta seq range)",
            RebuildPath::None,
        ),
        // Resumable super-node purge (ADR 0021)
        region(
            "PENDING_VERTEX_PURGES",
            40,
            StableMemoryClass::Maintenance,
            "vertex delete",
            "Vertices tombstoned by DETACH DELETE whose incident edges are still draining",
            RebuildPath::None,
        ),
        // Federated index repair journal (ADR 0023 D5)
        region(
            "INDEX_REPAIR_JOURNAL",
            41,
            StableMemoryClass::Maintenance,
            "federated index repair",
            "Failed-flush index postings persisted on compensation-success, re-applied by the maintenance driver and on post_upgrade",
            RebuildPath::None,
        ),
        // Cross-shard uniqueness: pinned unique-effect outbox (ADR 0030)
        region(
            "UNIQUE_EFFECT_OUTBOX",
            42,
            StableMemoryClass::Canonical,
            "cross-shard uniqueness",
            "EffectId → UniqueEffectReceipt (Acquire/Release); pinned commit evidence until the \
             Router acks. Canonical: un-acked effect absence is authoritative proof of non-commit, \
             decoupled from the ADR 0027 journal retention",
            RebuildPath::None,
        ),
        // ShardLocalGlobal unique fast path: shard-local unique value table (ADR 0030 slice 10)
        region(
            "GRAPH_LOCAL_UNIQUE_VALUES",
            43,
            StableMemoryClass::Canonical,
            "shard-local global uniqueness",
            "(constraint_id, encoded_value) → LocalUniqueRecord { owner_element_id }: graph-wide \
             unique values for single-shard graphs enforced entirely in the owning shard, bypassing \
             the federated reservation/outbox path. Canonical source of truth for ShardLocalGlobal \
             constraints; freed by owner-matched release, drained by the DROP purge",
            RebuildPath::None,
        ),
        // Canonical vertex embeddings (ADR 0031)
        region(
            "VERTEX_EMBEDDINGS",
            44,
            StableMemoryClass::Canonical,
            "embeddings",
            "(VertexId, EmbeddingNameId) → StoredEmbedding { encoding, dims, version, bytes }: \
             canonical fixed-dimension F32 vertex embeddings owned by the graph shard; source for \
             future derived vector-index backfill",
            RebuildPath::None,
        ),
        // Delete-spanning embedding incarnation high-water marks (ADR 0031 Slice 4)
        region(
            "VERTEX_EMBEDDING_INCARNATIONS",
            45,
            StableMemoryClass::Canonical,
            "embedding incarnations",
            "(VertexId, EmbeddingNameId) → u64: delete-spanning incarnation high-water mark per \
             embedding identity. Retained across remove so a reinsert allocates a strictly greater \
             incarnation; the vector canister orders derived sync by (incarnation, version) so a \
             stale remove can never tombstone a newer live vector",
            RebuildPath::None,
        ),
    ],
};

/// Router canister — registry, catalogs, telemetry, backfill. Baseline: ADR 0007 §2.
pub static ROUTER_STABLE_LAYOUT: StableCanisterLayout = StableCanisterLayout {
    canister: "router",
    regions: &[
        region(
            "ROUTER_AUTH_PRINCIPAL_RECORDS",
            0,
            StableMemoryClass::Canonical,
            "auth",
            "Per-principal RBAC / prepared-query auth records",
            RebuildPath::None,
        ),
        region(
            "ROUTER_GRAPHS",
            1,
            StableMemoryClass::Canonical,
            "registry",
            "GraphId → registry entry",
            RebuildPath::None,
        ),
        region(
            "ROUTER_SHARDS",
            2,
            StableMemoryClass::Canonical,
            "registry",
            "(GraphId, ShardId) → shard registry entry (ADR 0019)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_SHARD_BY_GRAPH",
            3,
            StableMemoryClass::Derived,
            "registry",
            "Graph canister principal → GraphShardKey — denormalized from ROUTER_SHARDS, commit-synced (ADR 0019)",
            RebuildPath::SyncCoUpdate,
        ),
        region(
            "ROUTER_SHARDS_BY_GRAPH_ID",
            4,
            StableMemoryClass::Derived,
            "registry",
            "GraphId → shard id list — denormalized fan-out index from ROUTER_SHARDS, commit-synced (ADR 0011)",
            RebuildPath::SyncCoUpdate,
        ),
        region(
            "ROUTER_GRAPH_RUNTIME_CONFIG",
            5,
            StableMemoryClass::Canonical,
            "runtime config",
            "GraphId → runtime config (element id encoding key, index cluster; ADR 0019 S2b/S3)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_MUTATION_COUNTER",
            6,
            StableMemoryClass::Canonical,
            "idempotency",
            "Monotonic router mutation id allocator",
            RebuildPath::None,
        ),
        region(
            "ROUTER_MUTATION_BY_CLIENT_KEY",
            7,
            StableMemoryClass::Canonical,
            "idempotency",
            "Client mutation key → router mutation record",
            RebuildPath::None,
        ),
        region(
            "ROUTER_PREPARED_PLANS",
            8,
            StableMemoryClass::Canonical,
            "prepared queries",
            "PreparedPlanKey → versioned plan wire blob",
            RebuildPath::None,
        ),
        region(
            "ROUTER_VERTEX_LABEL_BY_NAME",
            9,
            StableMemoryClass::Catalog,
            "resolution",
            "Vertex label (GraphId, name) → VertexLabelId (ADR 0018)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_VERTEX_LABEL_BY_ID",
            10,
            StableMemoryClass::Catalog,
            "resolution",
            "VertexLabelId → (GraphId, label name) (ADR 0018)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_EDGE_LABEL_BY_NAME",
            11,
            StableMemoryClass::Catalog,
            "resolution",
            "Edge label (GraphId, name) → EdgeLabelId (ADR 0018)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_EDGE_LABEL_BY_ID",
            12,
            StableMemoryClass::Catalog,
            "resolution",
            "EdgeLabelId → (GraphId, label name) (ADR 0018)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_PROPERTY_BY_NAME",
            13,
            StableMemoryClass::Catalog,
            "resolution",
            "Property (GraphId, name) → PropertyId (ADR 0018)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_PROPERTY_BY_ID",
            14,
            StableMemoryClass::Catalog,
            "resolution",
            "PropertyId → (GraphId, property name) (ADR 0018)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_GRAPH_BY_NAME",
            15,
            StableMemoryClass::Catalog,
            "resolution",
            "Graph name → GraphId (ADR 0011)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_GRAPH_BY_ID",
            16,
            StableMemoryClass::Catalog,
            "resolution",
            "GraphId → graph name (ADR 0011)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_INDEX_NAME_BY_NAME",
            17,
            StableMemoryClass::Catalog,
            "resolution",
            "Graph-scoped index name → IndexNameId (ADR 0011)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_INDEX_NAME_BY_ID",
            18,
            StableMemoryClass::Catalog,
            "resolution",
            "Graph-scoped IndexNameId → index name (ADR 0011)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_NAMED_INDEXES",
            19,
            StableMemoryClass::Catalog,
            "index planner catalog",
            "(graph_id, index_name_id) → IndexDefRecord (ADR 0009 DDL metadata)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_INDEXED_PROPERTY_SET",
            20,
            StableMemoryClass::Catalog,
            "index planner catalog",
            "(graph_id, kind, property_id) membership for planner + shard fan-out",
            RebuildPath::None,
        ),
        region(
            "ROUTER_EDGE_PAYLOAD_PROFILES",
            21,
            StableMemoryClass::Catalog,
            "edge payload schema",
            "(GraphId, EdgeLabelId) → EdgePayloadProfile (ADR 0008, ADR 0018)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_GRAPH_TYPE_DEFINITIONS",
            22,
            StableMemoryClass::Catalog,
            "graph type catalog",
            "GraphTypeId → graph type definition (ADR 0014)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_GRAPH_SCHEMA_BINDINGS",
            23,
            StableMemoryClass::Catalog,
            "graph type catalog",
            "GraphId → property graph schema binding (ADR 0013)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_GRAPH_TYPE_BY_NAME",
            24,
            StableMemoryClass::Catalog,
            "graph type name catalog",
            "Graph type name → GraphTypeId (ADR 0014)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_GRAPH_TYPE_BY_ID",
            25,
            StableMemoryClass::Catalog,
            "graph type name catalog",
            "GraphTypeId → graph type name (ADR 0014)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_VERTEX_LABEL_STATS",
            26,
            StableMemoryClass::Telemetry,
            "label telemetry",
            "(GraphId, VertexLabelId) aggregated usage stats",
            RebuildPath::Named("admin_label_stats_projection_step"),
        ),
        region(
            "ROUTER_EDGE_LABEL_STATS",
            27,
            StableMemoryClass::Telemetry,
            "label telemetry",
            "(GraphId, EdgeLabelId) aggregated usage stats",
            RebuildPath::Named("admin_label_stats_projection_step"),
        ),
        region(
            "ROUTER_VERTEX_LABEL_LIVE_BY_SHARD",
            28,
            StableMemoryClass::Telemetry,
            "label telemetry",
            "(GraphId, ShardId, VertexLabelId) live vertex counts",
            RebuildPath::Named("admin_label_stats_projection_step"),
        ),
        region(
            "ROUTER_EDGE_LABEL_LIVE_BY_SHARD",
            29,
            StableMemoryClass::Telemetry,
            "label telemetry",
            "(GraphId, ShardId, EdgeLabelId) live edge counts",
            RebuildPath::Named("admin_label_stats_projection_step"),
        ),
        region(
            "ROUTER_LABEL_STATS_PROJECTION",
            30,
            StableMemoryClass::Telemetry,
            "label stats projection",
            "GraphShardKey → applied_through_seq for graph label stats deltas (ADR 0015, 0019)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_LABEL_BACKFILL_STATE",
            31,
            StableMemoryClass::Maintenance,
            "label backfill",
            "GraphShardKey → cursor for label posting backfill admin",
            RebuildPath::None,
        ),
        region(
            "ROUTER_VERTEX_PROPERTY_BACKFILL_STATE",
            32,
            StableMemoryClass::Maintenance,
            "vertex property backfill",
            "GraphShardKey → cursor for vertex property posting backfill admin",
            RebuildPath::None,
        ),
        region(
            "ROUTER_EDGE_BACKFILL_STATE",
            33,
            StableMemoryClass::Maintenance,
            "edge backfill",
            "GraphShardKey → cursor for edge property posting backfill admin",
            RebuildPath::None,
        ),
        region(
            "ROUTER_CONSTRAINT_NAME_BY_NAME",
            34,
            StableMemoryClass::Catalog,
            "constraint name catalog",
            "Graph-scoped constraint name → ConstraintNameId (ADR 0030)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_CONSTRAINT_NAME_BY_ID",
            35,
            StableMemoryClass::Catalog,
            "constraint name catalog",
            "Graph-scoped ConstraintNameId → constraint name (ADR 0030)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_UNIQUE_CONSTRAINTS",
            36,
            StableMemoryClass::Catalog,
            "uniqueness constraint catalog",
            "(graph_id, constraint_name_id) → ConstraintDefRecord (ADR 0030)",
            RebuildPath::None,
        ),
        region(
            "ROUTER_UNIQUE_RESERVATIONS",
            37,
            StableMemoryClass::Canonical,
            "uniqueness reservation table",
            "(graph_id, constraint_id, encoded_value) → reservation (TCC claim state; ADR 0030). \
             Canonical: a Reserved-not-yet-committed claim has no outbox receipt to rebuild from",
            RebuildPath::None,
        ),
        region(
            "ROUTER_MUTATION_RESERVATION_INDEX",
            38,
            StableMemoryClass::Canonical,
            "uniqueness reservation reverse index",
            "mutation_id → (ClientMutationKey, nonterminal reservation count) (ADR 0030 slice 6). \
             Canonical: GC-pins the owning mutation record while non-terminal reservations remain \
             and resolves a reservation claim to its record for reclaim",
            RebuildPath::None,
        ),
        region(
            "ROUTER_UNIQUE_EFFECT_PENDING",
            39,
            StableMemoryClass::Canonical,
            "pending unique-effect discovery index",
            "(graph_id, mutation_id, shard_id) → pinned graph canister (ADR 0030 slice 6). \
             Canonical: the durable discovery source for Driver 2's unified effect recovery, \
             including orphan Acquires no reservation can find; registered before the first \
             dispatch await and removed only after the shard re-enumerates empty",
            RebuildPath::None,
        ),
        region(
            "ROUTER_EMBEDDING_NAME_BY_NAME",
            40,
            StableMemoryClass::Catalog,
            "embedding name resolution",
            "(graph_id, name) → EmbeddingNameId (ADR 0031). Router is the sole allocator of \
             embedding name ids the graph stamps on canonical embedding writes",
            RebuildPath::None,
        ),
        region(
            "ROUTER_EMBEDDING_NAME_BY_ID",
            41,
            StableMemoryClass::Catalog,
            "embedding name resolution",
            "(graph_id, EmbeddingNameId) → name (ADR 0031); reverse direction of the embedding \
             name catalog",
            RebuildPath::None,
        ),
        region(
            "ROUTER_VECTOR_INDEXES",
            42,
            StableMemoryClass::Catalog,
            "derived vector index catalog",
            "(graph_id, index_id) → VectorIndexDefRecord (ADR 0031 Slice 3). Definition, single \
             target, and fail-closed activation state for a derived vector index",
            RebuildPath::None,
        ),
        region(
            "ROUTER_VECTOR_DISPATCH_ACTIVATION",
            43,
            StableMemoryClass::Canonical,
            "global vector-dispatch activation flag",
            "Cell<bool> (ADR 0031 Slice 4). Operator-owned, reversible global switch gating all \
             production derived-vector dispatch/backfill; defaults false (fail-closed)",
            RebuildPath::None,
        ),
    ],
};

/// Graph-index canister. Baseline: ADR 0007 §2.
pub static INDEX_STABLE_LAYOUT: StableCanisterLayout = StableCanisterLayout {
    canister: "graph-index",
    regions: &[
        region(
            "INDEX_ROUTER",
            0,
            StableMemoryClass::Canonical,
            "router authorization",
            "Authorized router canister principal cell",
            RebuildPath::None,
        ),
        region(
            "INDEX_SHARD_CANISTER_BY_SHARD",
            1,
            StableMemoryClass::Canonical,
            "shard canister catalog",
            "ShardId → graph shard canister principal",
            RebuildPath::None,
        ),
        region(
            "INDEX_SHARD_BY_CANISTER",
            2,
            StableMemoryClass::Canonical,
            "shard canister catalog",
            "Graph shard canister principal → ShardId",
            RebuildPath::None,
        ),
        region(
            "INDEX_OWNERSHIP_CONFIG",
            3,
            StableMemoryClass::Canonical,
            "graph ownership",
            "Index canister graph ownership and shard-group config (ADR 0019 S4)",
            RebuildPath::None,
        ),
        region(
            "INDEX_VERTEX_POSTINGS",
            4,
            StableMemoryClass::Derived,
            "property postings",
            "Global property equality/range posting set",
            RebuildPath::Named("backfill_vertex_property_postings"),
        ),
        region(
            "INDEX_VERTEX_LABEL_POSTINGS",
            5,
            StableMemoryClass::Derived,
            "vertex label postings",
            "Global vertex label membership posting set",
            RebuildPath::Named("backfill_label_postings"),
        ),
        region(
            "INDEX_EDGE_POSTINGS",
            6,
            StableMemoryClass::Derived,
            "edge property postings",
            "Global edge property equality posting set (ADR 0009)",
            RebuildPath::Named("backfill_edge_property_postings"),
        ),
    ],
};

/// Graph-vector-index canister — degenerate `ivf_flat` derived index (ADR 0031 Slice 2).
///
/// Canonical regions (router auth, shard catalog, ownership, index defs) mirror `graph-index`.
/// All search/page structures are `Derived`: durable derived index state rebuildable from the
/// graph canonical [`VERTEX_EMBEDDINGS`](GRAPH_STABLE_LAYOUT) store via `vertex_embedding_backfill`,
/// not a deletable cache. `IVF_CENTROIDS` (MemoryId 6) is reserved empty in Slice 2 to avoid a
/// future `MemoryId` repack when Slice 4 populates centroid bytes.
pub static VECTOR_INDEX_STABLE_LAYOUT: StableCanisterLayout = StableCanisterLayout {
    canister: "graph-vector-index",
    regions: &[
        region(
            "VECTOR_INDEX_ROUTER",
            0,
            StableMemoryClass::Canonical,
            "router authorization",
            "Authorized router canister principal cell",
            RebuildPath::None,
        ),
        region(
            "VECTOR_INDEX_SHARD_CANISTER_BY_SHARD",
            1,
            StableMemoryClass::Canonical,
            "shard canister catalog",
            "ShardId → graph shard canister principal",
            RebuildPath::None,
        ),
        region(
            "VECTOR_INDEX_SHARD_BY_CANISTER",
            2,
            StableMemoryClass::Canonical,
            "shard canister catalog",
            "Graph shard canister principal → ShardId",
            RebuildPath::None,
        ),
        region(
            "VECTOR_INDEX_OWNERSHIP_CONFIG",
            3,
            StableMemoryClass::Canonical,
            "graph ownership",
            "Vector-index canister graph ownership and shard-group config (ADR 0019 S4)",
            RebuildPath::None,
        ),
        region(
            "VECTOR_INDEX_DEFS",
            4,
            StableMemoryClass::Canonical,
            "vector index definitions",
            "index_id → { kind: ivf_flat, encoding, dims, metric, nlist: 1, active_index_version, \
             stride_bytes, max_page_bytes, slots_per_page, next_vector_id }: authoritative index \
             config + durable VectorId allocator",
            RebuildPath::None,
        ),
        region(
            "IVF_CENTROID_META",
            5,
            StableMemoryClass::Derived,
            "ivf centroid metadata",
            "Degenerate centroid cache/training state (nlist=1, centroid-not-ready); holds only \
             centroid-specific derived state, never restates active_index_version/nlist",
            RebuildPath::Named("vertex_embedding_backfill"),
        ),
        region(
            "IVF_CENTROIDS",
            6,
            StableMemoryClass::Derived,
            "ivf centroids",
            "Reserved empty in Slice 2; (index_id, index_version, partition_id) → centroid bytes \
             once Slice 4 trains centroids",
            RebuildPath::Named("vertex_embedding_backfill"),
        ),
        region(
            "VECTOR_SUBJECT_TO_ID",
            7,
            StableMemoryClass::Derived,
            "subject map",
            "(index_id, subject) → SubjectMapEntry { embedding_incarnation, \
             stored_embedding_version, deleted, slot, vector_id }: retained as a durable clock \
             after delete; ordered by (embedding_incarnation, stored_embedding_version) so a stale \
             remove cannot tombstone a newer reinsert and a stale upsert cannot resurrect a removed \
             vector (ADR 0031 Slice 4)",
            RebuildPath::Named("vertex_embedding_backfill"),
        ),
        region(
            "VECTOR_ID_TO_SLOT",
            8,
            StableMemoryClass::Derived,
            "vector id index",
            "(index_id, vector_id) → SlotRef { index_version, partition_id, page_id, slot, \
             generation }",
            RebuildPath::Named("vertex_embedding_backfill"),
        ),
        region(
            "VECTOR_PARTITION_HEADS",
            9,
            StableMemoryClass::Derived,
            "partition heads",
            "(index_id, index_version, partition_id) → head { first_page, mutable_page, \
             page_count, live_len, next_page_id }: durable page allocator per partition/version",
            RebuildPath::Named("vertex_embedding_backfill"),
        ),
        region(
            "VECTOR_PAGE",
            10,
            StableMemoryClass::Derived,
            "vector pages",
            "(index_id, index_version, partition_id, page_id) → fixed-capacity page blob of vector \
             rows; fullness bounded by slots_per_page from the index def",
            RebuildPath::Named("vertex_embedding_backfill"),
        ),
        region(
            "VECTOR_ID_TO_SUBJECT",
            11,
            StableMemoryClass::Derived,
            "vector id reverse map",
            "(index_id, vector_id) → VectorSubject: reverse locator for partition-page search \
             (ADR 0031 Slice 6); VECTOR_SUBJECT_TO_ID remains the freshness source of truth",
            RebuildPath::Named("vertex_embedding_backfill"),
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
                // Legacy read view: another store owns writes, so it has no rebuild path.
                if !matches!(region.rebuild, RebuildPath::None) {
                    return Err(ClassInvariantError::CompatibilityWithRebuild {
                        canister: layout.canister,
                        symbol: region.symbol,
                    });
                }
            }
            StableMemoryClass::Canonical
            | StableMemoryClass::Maintenance
            | StableMemoryClass::Catalog => {
                // Authoritative facts, physical bookkeeping, and resolution catalogs are not
                // reconstructed from other stable state.
                if !matches!(region.rebuild, RebuildPath::None) {
                    return Err(ClassInvariantError::UnexpectedRebuild {
                        canister: layout.canister,
                        symbol: region.symbol,
                        class: region.class,
                    });
                }
            }
            StableMemoryClass::Derived => {
                // Every derived region must declare how it stays consistent: a named rebuild /
                // backfill, or explicit sync co-update. An unspecified `None` is a layout bug.
                if matches!(region.rebuild, RebuildPath::None) {
                    return Err(ClassInvariantError::DerivedWithoutRebuild {
                        canister: layout.canister,
                        symbol: region.symbol,
                    });
                }
            }
            StableMemoryClass::Telemetry => {
                // Specialized derived state: aggregates name a projection step; event source logs
                // and cursors carry `None`. Telemetry is never a sync co-update mirror.
                if matches!(region.rebuild, RebuildPath::SyncCoUpdate) {
                    return Err(ClassInvariantError::UnexpectedRebuild {
                        canister: layout.canister,
                        symbol: region.symbol,
                        class: region.class,
                    });
                }
            }
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
        assert_eq!(GRAPH_STABLE_LAYOUT.region_count(), 46);
        assert_eq!(GRAPH_STABLE_LAYOUT.max_memory_id(), Some(45));
        assert_eq!(GRAPH_STABLE_LAYOUT.regions[0].symbol, "FWD_VERTICES");
        assert_eq!(
            GRAPH_STABLE_LAYOUT.regions[39].symbol,
            "GRAPH_MUTATION_JOURNAL"
        );
        assert_eq!(
            GRAPH_STABLE_LAYOUT.regions[40].symbol,
            "PENDING_VERTEX_PURGES"
        );
        assert_eq!(
            GRAPH_STABLE_LAYOUT.regions[41].symbol,
            "INDEX_REPAIR_JOURNAL"
        );
        assert_eq!(
            GRAPH_STABLE_LAYOUT.regions[43].symbol,
            "GRAPH_LOCAL_UNIQUE_VALUES"
        );
        assert_eq!(GRAPH_STABLE_LAYOUT.regions[44].symbol, "VERTEX_EMBEDDINGS");
        assert_eq!(
            GRAPH_STABLE_LAYOUT.regions[45].symbol,
            "VERTEX_EMBEDDING_INCARNATIONS"
        );
        assert_eq!(
            GRAPH_STABLE_LAYOUT.regions[35].class,
            StableMemoryClass::Derived
        );
    }

    #[test]
    fn router_layout_registry_matches_baseline() {
        assert_layout(&ROUTER_STABLE_LAYOUT);
        assert_eq!(ROUTER_STABLE_LAYOUT.region_count(), 44);
        assert_eq!(ROUTER_STABLE_LAYOUT.max_memory_id(), Some(43));
        assert_eq!(
            ROUTER_STABLE_LAYOUT.regions[30].class,
            StableMemoryClass::Telemetry
        );
        assert_eq!(
            ROUTER_STABLE_LAYOUT.regions[30].symbol,
            "ROUTER_LABEL_STATS_PROJECTION"
        );
    }

    /// Registry dispatch SSOT vs denormalized fan-out indexes must keep the canonical/derived
    /// split documented in `stable-memory-inventory.md` (router registry section, ADR 0011/0019).
    #[test]
    fn router_registry_canonical_derived_split_matches_inventory() {
        let region_of = |symbol: &str| {
            *ROUTER_STABLE_LAYOUT
                .regions
                .iter()
                .find(|region| region.symbol == symbol)
                .unwrap_or_else(|| panic!("router layout missing {symbol}"))
        };
        // `ROUTER_SHARDS` is the shard dispatch source of truth.
        let shards = region_of("ROUTER_SHARDS");
        assert_eq!(shards.class, StableMemoryClass::Canonical);
        assert_eq!(shards.rebuild, RebuildPath::None);
        // Both lookup projections are denormalized from `ROUTER_SHARDS`; commit-synced, not SSOT.
        for symbol in ["ROUTER_SHARD_BY_GRAPH", "ROUTER_SHARDS_BY_GRAPH_ID"] {
            let projection = region_of(symbol);
            assert_eq!(projection.class, StableMemoryClass::Derived, "{symbol}");
            assert_eq!(
                projection.rebuild,
                RebuildPath::SyncCoUpdate,
                "{symbol} is commit-synced, not a named rebuild"
            );
        }
    }

    /// Every `Derived` region must declare a rebuild path (named or sync co-update); an
    /// unspecified `None` is rejected so the canonical/derived contract cannot silently regress.
    #[test]
    fn derived_regions_declare_a_rebuild_path() {
        for layout in [
            &GRAPH_STABLE_LAYOUT,
            &ROUTER_STABLE_LAYOUT,
            &INDEX_STABLE_LAYOUT,
            &VECTOR_INDEX_STABLE_LAYOUT,
        ] {
            for region in layout.regions {
                if region.class == StableMemoryClass::Derived {
                    assert_ne!(
                        region.rebuild,
                        RebuildPath::None,
                        "{}::{} is Derived but declares no rebuild path",
                        layout.canister,
                        region.symbol
                    );
                }
            }
        }
        // LARA reverse adjacency and edge aliases name a rebuild; index postings name a backfill.
        let graph = |symbol: &str| {
            GRAPH_STABLE_LAYOUT
                .regions
                .iter()
                .find(|r| r.symbol == symbol)
                .unwrap()
                .rebuild
        };
        assert_eq!(
            graph("REV_VERTICES"),
            RebuildPath::Named("rebuild_reverse_adjacency")
        );
        assert_eq!(
            graph("EDGE_ALIASES"),
            RebuildPath::Named("rebuild_edge_aliases")
        );
        assert_eq!(
            INDEX_STABLE_LAYOUT.regions[4].rebuild,
            RebuildPath::Named("backfill_vertex_property_postings")
        );
    }

    /// A `Derived` region carrying `RebuildPath::None` is a layout bug and must be rejected.
    #[test]
    fn derived_without_rebuild_is_rejected() {
        static BAD: StableCanisterLayout = StableCanisterLayout {
            canister: "bad",
            regions: &[region(
                "BAD_DERIVED",
                0,
                StableMemoryClass::Derived,
                "test",
                "derived region with no declared rebuild path",
                RebuildPath::None,
            )],
        };
        assert_eq!(
            validate_class_invariants(&BAD),
            Err(ClassInvariantError::DerivedWithoutRebuild {
                canister: "bad",
                symbol: "BAD_DERIVED",
            })
        );
    }

    #[test]
    fn index_layout_registry_matches_baseline() {
        assert_layout(&INDEX_STABLE_LAYOUT);
        assert_eq!(INDEX_STABLE_LAYOUT.region_count(), 7);
        assert_eq!(INDEX_STABLE_LAYOUT.max_memory_id(), Some(6));
    }

    #[test]
    fn vector_index_layout_registry_matches_baseline() {
        assert_layout(&VECTOR_INDEX_STABLE_LAYOUT);
        assert_eq!(VECTOR_INDEX_STABLE_LAYOUT.region_count(), 12);
        assert_eq!(VECTOR_INDEX_STABLE_LAYOUT.max_memory_id(), Some(11));
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.regions[4].symbol,
            "VECTOR_INDEX_DEFS"
        );
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.regions[4].class,
            StableMemoryClass::Canonical
        );
        // IVF_CENTROIDS is reserved empty in Slice 2 but already classified derived.
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.regions[6].symbol,
            "IVF_CENTROIDS"
        );
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.regions[6].class,
            StableMemoryClass::Derived
        );
        // Subject map is durable derived state (tombstone clock), not a deletable cache.
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.regions[7].symbol,
            "VECTOR_SUBJECT_TO_ID"
        );
        assert_eq!(VECTOR_INDEX_STABLE_LAYOUT.regions[10].symbol, "VECTOR_PAGE");
        // ADR 0031 Slice 6: reverse locator for partition-page search, derived/rebuildable.
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.regions[11].symbol,
            "VECTOR_ID_TO_SUBJECT"
        );
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.regions[11].class,
            StableMemoryClass::Derived
        );
    }

    #[test]
    fn stable_memory_class_inventory_names() {
        assert_eq!(StableMemoryClass::Canonical.inventory_name(), "canonical");
        assert_eq!(StableMemoryClass::Telemetry.inventory_name(), "telemetry");
    }
}
