# 0009. Edge property index on graph-index, mixed intersection, and opt-in index DDL

Date: 2026-06-12  
Status: accepted  
Last revised: 2026-06-13  
Anchor timestamp: 2026-06-13 06:18:40 UTC +0000

## Revision history

| Date | Change |
|------|--------|
| 2026-06-12 | Proposed; edge postings on graph-index, mixed intersection, opt-in `CREATE INDEX` / `DROP INDEX` DDL. |
| 2026-06-12 | Accepted; policy frozen pending implementation phases A–E in §Implementation phases. |
| 2026-06-12 | Phase A implemented: shard index registry, opt-in DML gate, router fan-out admin APIs. |
| 2026-06-12 | Phase B implemented: `INDEX_EDGE_POSTINGS` on graph-index; federated edge DML flush; edge backfill API. |
| 2026-06-12 | Phase C implemented: `IndexSubject` / `IndexIntersectionResult`; mixed vertex/edge intersection on graph-index. |
| 2026-06-12 | Phase D (partial): router `EdgeIndexScan` / all-edge intersection seeds; `LocalEdgePosting` wire; graph edge seed apply + skip leading `EdgeIndexScan`. `EDGE_EQUALITY_POSTINGS` retire pending. |
| 2026-06-12 | Phase D implemented: retired graph `EDGE_EQUALITY_POSTINGS`; MemoryId repack (40 regions); expand/edge scan via graph-index client, router seeds, or `EDGE_PROPERTIES` scan fallback. |
| 2026-06-12 | Phase E implemented: router `CREATE INDEX` / `DROP INDEX` extension DDL via `gql_execute*`; named index catalog; shard `unregister_indexed_property`. |
| 2026-06-12 | Index catalog stable layout: row-oriented `ROUTER_NAMED_INDEXES` + `ROUTER_INDEXED_PROPERTY_SET` with `PropertyId` / label ids (replaces per-graph Candid blob). |
| 2026-06-13 | Planner stats: `RouterGraphStats` loads `PropertyId` membership; `GraphStats` adapter resolves names via property catalog; one stats load per GQL execution. |
| 2026-06-13 | Phase E PocketIC e2e: `DROP INDEX` standalone scan fallback + federated anchor loss; planner `PropertyFilter`/`Filter` contribute to `property_uses` for shard `resolved_properties`. |
| 2026-06-13 | Phase E PocketIC e2e: edge `CREATE INDEX` / `DROP INDEX` via `e2e_insert_directed_edge_with_property`; standalone scan fallback and federated anchor loss for `()-[e:L {p: v}]->` queries. |
| 2026-06-13 | [ADR 0012](0012-edge-index-direction-in-ddl.md) proposed: GQL `EdgeDirection` in edge `FOR`; graph-index `wire_label_id` keys; planner storage-class subset rule (amends §1 `label_id`, §4 edge DDL). |

## Context

ADR [0006](0006-pre-federation-foundation.md) places **vertex** property equality postings on
**graph-index** and makes the **router** the reader for anchor routing
([federation-target.md](../sharding/federation-target.md)). **Edge** property equality today lives
on each graph shard as derived stable `EDGE_EQUALITY_POSTINGS`
([property-index.md](../index/property-index.md) § graph shard local indexes).

### Problems today

| Area | Issue |
|------|--------|
| **Asymmetric ownership** | Vertex anchors use `lookup_equal` / `lookup_intersection` on graph-index; edge equality uses shard-local `EDGE_EQUALITY_POSTINGS` during Expand only. |
| **No federated edge anchor** | Leading `EdgeIndexScan` (`()-[e:L {p: v}]->(b)`) cannot be resolved once per logical graph; each shard maintains its own posting set. |
| **No vertex ∩ edge intersection** | `lookup_intersection` accepts vertex property arms only (`PostingHit` = `(shard_id, vertex_id)`). Queries such as `WHERE a.age = 30 AND e.weight = 5` cannot narrow seeds in the index plane. |
| **Over-broad DML maintenance** | Graph DML enqueues vertex/edge postings for every **indexable** property value (`sortable_index_key`), while the planner uses indexes only when `RouterGraphStats` lists the property — **write/storage work without query benefit**. |
| **Admin surface** | Vertex: `admin_set_indexed_vertex_property` (canister API). Edge: `indexed_edge_properties` exists in stats types but has **no** admin/DDL path. ISO/IEC 39075 §12 primitive DDL has **no** `CREATE INDEX` ([gleaph-gql](../../crates/gql) implements SCHEMA/GRAPH/GRAPH TYPE only). |
| **Key shape** | Shard-local edge keys are `(property_id, value, owner, label, slot)` with **no** `shard_id` and **label after owner**, which weakens labeled prefix probes. |

### Prerequisites (met or in flight)

- graph-index property postings + `lookup_intersection` ([lookup-intersection.md](../index/lookup-intersection.md))
- Router `ROUTER_INDEXED_PROPERTIES` / `RouterGraphStats` ([planner_stats.rs](../../crates/router/src/planner_stats.rs))
- `GlobalEdgeId` / forward CSR owner convention ([0005](0005-vertex-identity.md))
- ADR [0008](0008-edge-payload-profile-router-ssot.md) — router catalog SSOT for label/property **ids** (orthogonal to index policy)

### Non-goals (this ADR)

- `CREATE INDEX … USING TEXT` / `VECTOR` (follow-up ADR or phase)
- `CREATE CONSTRAINT …` integrity rules (separate from performance indexes)
- Range predicates on edge properties in intersection v1 (equality only, matching vertex intersection)
- Reintroducing graph-shard index client on federated read hot path
- Production migration from pre-0009 snapshots (dev data discard per [refactoring-roadmap.md](../architecture/refactoring-roadmap.md))

---

## Decision

### 1. Edge property postings live on graph-index (SSOT for equality)

Move edge property equality postings from graph stable `EDGE_EQUALITY_POSTINGS` to **graph-index**
as a **separate** derived store (distinct magic / `BTreeSet` from vertex `PostingKey`; do not mix
entity kinds in one key space without an explicit tag).

**Canonical posting key** (lexicographic, equality + prefix scans):

```text
(property_id, value, label_id, shard_id, owner_vertex_id, slot_index)
```

| Field | Role |
|-------|------|
| `property_id` | Router-issued `PropertyId` (same as vertex postings) |
| `value` | Sortable index key bytes (`value_to_index_key_bytes`) |
| `label_id` | `EdgeLabelId` raw; sentinel for unlabeled edges (see §1.1). **Amended by [ADR 0012](0012-edge-index-direction-in-ddl.md):** store LARA **`wire_label_id`** (`BucketLabelKey` raw, directed MSB included) in graph-index keys; catalog id remains in router registry together with `EdgeDirection`. |
| `shard_id` | Owning graph shard |
| `owner_vertex_id` | Forward CSR owner (`VertexId` on that shard) |
| `slot_index` | Edge slot within labeled adjacency |

**Invariants**

- One posting per indexed `(property, value)` snapshot on an edge identity; DML insert/remove mirrors vertex index rules.
- **Canonical owner only** — reverse/undirected alias edges do not duplicate postings.
- Index does not read tombstones; graph DML must `posting_remove` on delete (same contract as vertex index).

#### 1.1 Unlabeled edges

Unlabeled edges use `label_id = 0` (reserved). DDL/planner MUST NOT register edge indexes on
properties for patterns without a catalog edge label unless a future ADR extends semantics.

#### 1.2 Retire graph `EDGE_EQUALITY_POSTINGS`

After cutover:

- Remove graph facade region `EDGE_EQUALITY_POSTINGS` (MemoryId repack per ADR [0007](0007-stable-memory-layout.md) gate).
- Expand `indexed_edge_equality` reads graph-index via router-supplied seeds or shard transition client — **not** shard stable lookup.
- Keep `EDGE_PROPERTIES` as canonical value store on the shard.

### 2. Opt-in indexes only (no implicit posting maintenance)

**Policy:** A property is indexed **only** when an administrator has registered it via index DDL
(§4). Graph DML MUST NOT insert edge (or vertex) postings for properties that are not registered for
that logical graph and entity kind.

| Layer | Responsibility |
|-------|----------------|
| **Router catalog** | SSOT: which `(entity, label?, property)` tuples are indexed per logical graph |
| **Planner** | `is_vertex_property_indexed` / `is_edge_property_indexed` from that catalog |
| **Graph DML** | `dispatch_property_index_ops` → enqueue vertex/edge ops **iff** property is registered |
| **graph-index** | Store postings only for registered properties (reject or no-op unknown ids — implementation choice; prefer no-op at index with catalog gate on graph) |

**Rationale:** Aligns write cost with query benefit; matches the usual operational model for explicit
secondary indexes; fixes
today’s divergence between maintenance (all indexable values) and planning (stats subset).

**Backfill:** Extend router `admin_property_backfill_step` / graph `backfill_property_postings` to
**registered vertex properties only**; add `admin_edge_property_backfill_step` (or unified step) for
registered edge properties replaying from `EDGE_PROPERTIES`.

### 3. Index-plane intersection (vertex, edge, mixed)

Extend graph-index beyond vertex-only `lookup_intersection`.

#### 3.1 Wire types (illustrative — `graph-kernel::index`)

```rust
pub enum IndexSubject {
    /// Vertex property equality; yields vertex hits.
    VertexProperty,
    /// Edge property equality; yields edge hits (or projected vertex hits).
    EdgeProperty { label_id: Option<u16> },
}

pub struct IndexEqualSpec {
    pub subject: IndexSubject,
    pub property_id: u32,
    pub value: Vec<u8>,
}

pub struct EdgePostingHit {
    pub shard_id: ShardId,
    pub owner_vertex_id: u32,
    pub label_id: u16,
    pub slot_index: u32,
}

pub enum IndexIntersectionResult {
    /// Existing anchor: start traversal at these vertices.
    Vertices(Vec<PostingHit>),
    /// Edge-led anchor: start at these edges (leading EdgeIndexScan).
    Edges(Vec<EdgePostingHit>),
}
```

v1 equality only; `specs.len() >= 2` for pure vertex arms unchanged. Single arm uses `lookup_equal` /
`lookup_edge_equal`.

#### 3.2 Algorithms

For each spec, range-scan the appropriate store with prefix:

| Subject | Prefix |
|---------|--------|
| `VertexProperty` | `(property_id, value)` → keys `(…, shard_id, vertex_id)` |
| `EdgeProperty { label_id: Some(L) }` | `(property_id, value, L)` |
| `EdgeProperty { label_id: None }` | `(property_id, value)` (all labels) |

Collect sets of **intersection keys**:

| Result kind | Intersection key per arm | Emitted hits |
|-------------|--------------------------|--------------|
| All vertex arms | `(shard_id, vertex_id)` | `PostingHit` |
| Mixed vertex + edge | `(shard_id, vertex_id)` with edge arm **projected** to `owner_vertex_id` | `PostingHit` (seed for expand source) |
| All edge arms (same label policy) | `(shard_id, owner, label, slot)` | `EdgePostingHit` |

**Complexity:** O(Σ |posting_i|); no graph canister calls (same as [lookup-intersection.md](../index/lookup-intersection.md)).

#### 3.3 Router integration

- Extend `IndexAnchor::from_plans` to recognize edge-led anchors and mixed multi-property patterns
  (planner already emits `indexed_edge_equality` / `EdgeIndexScan`; wire seeds accordingly).
- Slice hits by `shard_id`; encode `seed_bindings_blob` for vertices or edge identities per plan op.
- Graph shards skip leading anchor ops when seeds present (existing `skip_leading_index_anchor_ops`
  pattern).

**Expand filter** remains valid: when the source vertex is already bound, executor may still filter
incident edges locally without an index round-trip; index intersection is for **plan prefix**
selectivity, not a replacement for all Expand paths.

### 4. Gleaph index DDL (ISO 39075 extension)

Introduce **Gleaph catalog extension DDL** parsed and executed on the **router** (not in `gleaph-gql`
core — project-specific extension module, e.g. `router` GQL admin path or `gleaph-gql-extensions`).

Syntax (pattern-based; names resolve via router catalogs):

```gql
-- Vertex property (graph-index vertex postings)
CREATE INDEX person_age IF NOT EXISTS
  FOR (n:Person) ON (n.age);

-- Edge property (graph-index edge postings); label required in pattern
-- Direction follows GQL EdgeDirection (see ADR 0012 for all seven forms).
CREATE INDEX knows_weight IF NOT EXISTS
  FOR ()-[e:KNOWS]-() ON (e.weight);

DROP INDEX person_age IF EXISTS;
DROP INDEX knows_weight IF EXISTS;
```

**Rules**

| Rule | Detail |
|------|--------|
| **Authorization** | Router controller / Manager+ role per [rbac-and-prepared.md](../security/rbac-and-prepared.md) |
| **Name resolution** | `Person`, `KNOWS`, property names interned via existing router catalogs → ids stored in index registry |
| **Index identity** | `index_name` unique per logical graph; maps to `(entity, label_id?, property_id)`; edge indexes also store **`EdgeDirection`** per [ADR 0012](0012-edge-index-direction-in-ddl.md) |
| **No side effects on CREATE GRAPH** | Creating a graph or graph type does **not** create indexes |
| **DROP** | Removes registry entry; optional async posting purge job or synchronous `posting_remove` scan per property (implementation phase; must complete before returning OK or document eventual consistency) |

`SHOW INDEXES` is a follow-up (informational); not required for ADR acceptance.

**Deprecation:** `admin_set_indexed_vertex_property` becomes a thin wrapper over the same catalog
entry as `CREATE INDEX … ON (n.prop)` or is removed after DDL lands.

### 5. Planner and executor contract (unchanged surface, new backend)

| Plan feature | Index backend after 0009 |
|--------------|---------------------------|
| `IndexScan` / `IndexIntersection` (vertex) | graph-index vertex postings; **registered properties only** |
| `indexed_edge_equality` / `EdgeIndexScan` | graph-index edge postings; router seeds or federated lookup |
| `edge_payload_predicate` (GLEAPH.WEIGHT bytes) | Unchanged — LARA inline payload path (ADR 0008), **not** property index |

---

## Implementation phases

| Phase | Deliverable | Verification |
|-------|-------------|--------------|
| **A — Registry + opt-in DML** | Router index registry (vertex + edge); gate `dispatch_property_index_ops` on registration; migrate `admin_set_indexed_vertex_property` | Unit tests: unregistered property writes no posting; registered does |
| **B — Edge postings on graph-index** | `EdgePostingKey` store; `posting_insert`/`remove` edge API; backfill from `EDGE_PROPERTIES` | graph-index tests; parity with vertex posting tests |
| **C — Lookup + mixed intersection** | `lookup_edge_equal`, extended `lookup_intersection`; `graph-kernel` types | graph-index + router client tests |
| **D — Router seeds + graph retire local** | Remove `EDGE_EQUALITY_POSTINGS`; expand/edge scan use seeds; MemoryId repack | pocket-ic; reopen; canbench delta |
| **E — Index DDL** | Parse/execute `CREATE INDEX` / `DROP INDEX`; RBAC; docs sync | planner fusion tests with DDL setup; PocketIC e2e (`router_gql_query`: vertex + edge CREATE/DROP, standalone scan fallback, federated anchor loss, idempotent/missing DROP) |

Phases A–B may land before D; **main** should not maintain two edge posting SSOTs beyond one merge window.

---

## Consequences

### Positive

- Vertex and edge property indexes share one canister and router read model.
- Mixed vertex/edge predicates can narrow seeds without scanning all incident edges.
- Administrators control index cardinality; DML and stable growth match query policy.
- Pattern-based `CREATE INDEX` / `DROP INDEX` DDL gives a portable, reviewable admin surface.

### Negative / costs

- Edge property DML may require inter-canister posting (latency vs today’s synchronous shard-local index).
- Additional graph-index stable region and backfill cursors.
- Graph MemoryId repack when removing `EDGE_EQUALITY_POSTINGS`.
- Extension DDL is not portable ISO 39075 strict mode.

### Risks

| Risk | Mitigation |
|------|------------|
| Posting lag vs canonical | Same pending/backfill contract as vertex index; document in derived-state semantics |
| DDL DROP leaves stale postings | Purge job or range delete by `(property_id)` prefix |
| Label omitted in DDL but required in query | Parser requires edge label in `FOR ()-[e:L]-()` pattern |
| Intersection projection too loose | Prefer specs with `label_id: Some(L)` when planner knows `L` |

---

## Alternatives considered

### A. Keep edge postings shard-local

**Rejected:** Blocks federated edge anchors and vertex∩edge intersection; perpetuates asymmetric ops model.

### B. `(property_id, value, shard_id, label_id, owner, slot)` key order

**Deferred:** Better for shard-scoped admin scans; worse for global labeled probe. Primary v1 access is `(prop, value, label?)` — label before shard (see discussion in ADR draft thread).

### C. Implicit indexes for all indexable properties

**Rejected:** Write amplification and stable bloat without administrator intent.

### D. ISO 39075 `CALL` catalog-modifying procedure only (no surface syntax)

**Rejected as sole surface:** Correct for standard purity but poor operability; pattern-based index DDL plus internal catalog is clearer for admins. Procedures may still implement DDL under the hood.

### E. Secondary maintenance index `(shard, owner, label, slot, …)`

**Deferred:** Add only if edge delete posting purge without canonical scan proves too slow.

---

## References

- [0005 — Vertex/edge identity](0005-vertex-identity.md)
- [0006 — Pre-federation foundation](0006-pre-federation-foundation.md)
- [0007 — Stable-memory layout](0007-stable-memory-layout.md)
- [property-index.md](../index/property-index.md)
- [lookup-intersection.md](../index/lookup-intersection.md)
- [federation-target.md](../sharding/federation-target.md)
- [stable-memory-inventory.md](../storage/stable-memory-inventory.md)
- [rbac-and-prepared.md](../security/rbac-and-prepared.md)
