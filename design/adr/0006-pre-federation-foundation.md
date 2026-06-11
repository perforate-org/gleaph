# 0006. Pre-federation foundation: shard identity, catalogs, storage layout, and vertex keys

Date: 2026-06-11  
Status: accepted  
Last revised: 2026-06-11

## Context

Gleaph’s codebase grew federation hooks (router placement, remote CSR edges, dual catalogs,
multi-shard PocketIC harness) ahead of a settled data model. Several choices are already in
stable memory while design docs still label them “deferred.” That mismatch makes it easy to add
orchestration on top of the wrong abstractions.

Phase 7 router seed dispatch and GPL wire decode are largely complete. Before more federation
work, we need a **coherent foundation** for:

- numeric shard identity and router registry;
- catalog ownership (names vs values);
- global vertex identity without `LogicalVertexId`;
- client-facing opaque element ids;
- index canister grouping policy;
- single-shard storage layout (what to keep, remove, repack).

This ADR is the umbrella policy. Wire-level encoding details for `ELEMENT_ID` and path ids are
expanded in [0005](0005-vertex-identity.md).

### Problems today

| Area | Issue |
|------|--------|
| **Shard id** | `ShardId` is a bare `u32` alias; tests use shards `7`/`9` instead of `0`/`1` |
| **Catalogs** | Router and graph each allocate `property_id` (and label ids) independently → index routing can silently break |
| **Vertex identity** | Redundant `LogicalVertexId` + `VERTEX_LOGICAL_IDS` when global key is `(shard_id, local_vertex_id)` |
| **Remote refs** | Stable maps target logical ids; `next_ref_id` is heap-only (upgrade-unsafe) |
| **Stable memory** | Federation-only regions (MemoryIds 36–41) allocated despite single-shard focus |
| **MemoryId layout** | Non-consecutive ids and reserved slots (e.g. router 14, graph gaps 30–35) |
| **Documentation** | `standalone-mode.md` says defer remote stable; inventory says implemented |

### Non-goals (this ADR)

- Production multi-shard `gql_query`, peer expand, remote CSR edge DML.
- Vertex migration placement state machines.
- Persistent `RemoteVertexId ↔ GlobalVertexId` table (define type only).
- Choosing final `GROUP_SIZE` for index canister groups.

---

## Decision

### 1. `ShardId` and shard registry

**Type:** transparent newtype `ShardId(u32)` in `graph-kernel`.

- **`Storable`:** delegate to inner `u32` (`self.0.to_le_bytes()` / `ShardId(u32::from_le_bytes(...))`).
- **`CandidType` / serde:** transparent encoding.
- **`0` is valid** — the sole shard in standalone mode is always **`ShardId(0)`**.

**Assignment (strategy A):** shards use contiguous ids **`0..n-1`**. One shard is the degenerate case
`n = 1` → only shard `0`. Future shards are issued incrementally (`1`, `2`, …) as federation
rolls out. Tests should use **`0` and `1`**, not arbitrary ids like `7`/`9`.

**Router** maintains the authoritative **`ShardId ↔ Principal`** registry (`ROUTER_SHARDS`,
`ShardRegistryEntry`: graph canister, index canister, logical graph name).

**Graph shards** store their own `shard_id` in metadata but **do not resolve other shard
Principals** until federation is implemented. No `PEER_GRAPH_CANISTERS` in the single-shard
layout.

**Invalid shard ids:** there is **no sentinel inside `ShardId`**. Unregistered ids are errors at
the router (`Option<ShardId>` for incomplete state; validate membership in `ROUTER_SHARDS` at
dispatch). Avoid `ShardId::default()` on partially-built structs where `0` could be mistaken for
“unset” — use `Option<ShardId>`.

**Posting vertex tail:** index `PostingKey` and `LabelPostingKey` identify vertices as
`(shard_id, local_vertex_id)` plus property/label key prefix. This is fixed for efficient shard
management without variable-length Principals in posting keys.

---

### 2. Catalog ownership (router SSOT)

**Router** is the single source of truth for **name → numeric id** of:

- vertex properties (`property_id`);
- vertex labels (`vertex_label_id`);
- edge labels (`edge_label_id`).

**Graph shard** receives ids from the router (physical plan, DML wire, or explicit intern before
write). It stores **values only**, keyed by `PropertyId` / label ids. It **does not** maintain
property or label **names** in stable storage.

| Layer | Owns |
|-------|------|
| Router | Names and numeric ids; planner resolution; `admin_intern_*` |
| Graph | `(property_id, Value)` on vertices/edges; CSR and property maps |
| Index | Postings `(property_id, value_bytes, shard_id, local_vertex_id)` and label postings |

**Remove** graph `PROPERTY_NAME_TO_ID` / `PROPERTY_ID_TO_NAME` (MemoryIds 25–26) once DML and
scan paths use router-assigned ids. Graph `get_or_insert_property_id` on the DML hot path is
replaced by router intern + graph write-by-id.

**Same rule** applies to vertex and edge labels: graph must not independently allocate label ids
in production paths (`resolve_plan_labels` / resolved label tables on the wire).

**Shared implementation:** keep `BidirectionalCatalog` in `graph-kernel` as the storage
abstraction; **ownership** is router-only for federated name resolution.

---

### 3. Global vertex identity (no `LogicalVertexId`)

**Canonical global vertex key:** **`GlobalVertexId { shard_id: ShardId, local_vertex_id: LocalVertexId }`**
(8 bytes LE). Subsumes today’s `PhysicalPlacementKey` / `PhysicalVertexLocation` pairing.

**Remove entirely:**

- `LogicalVertexId` type and `standalone_logical_vertex_id`
- `VERTEX_LOGICAL_IDS` (graph MemoryId 36)
- Router `ROUTER_LOGICAL_COUNTER`, `ROUTER_PENDING_LOGICAL`
- Router placement keyed by logical id; `ROUTER_PLACEMENT_BY_PHYSICAL` → logical reverse map
- APIs: `allocate_logical_vertex_id`, `resolve_placement(logical)`, `release_logical_vertex`

**Router placement (simplified):** graph inserts locally → `commit_vertex_placement { local_vertex_id }`
(shard from registration) → router records active **`GlobalVertexId`**. Resolve by physical key
only.

**Shard-internal remote pointer:** **`RemoteVertexId`** (rename of `RemoteRefId`) — 30-bit payload
in `VertexRef` with existing remote bit. **Never exported** on router, index, or client APIs.

- Allocate from **`1`**; **`0` never assigned** (allocator hygiene, not a public `INVALID` sentinel).
- Use **`Option<RemoteVertexId>`** at boundaries; tombstones remain `VertexRef::tombstone()`.
- **Deferred:** persistent mutual index `RemoteVertexId ↔ GlobalVertexId` per shard.
- **Deferred:** remote CSR edge creation, `REMOTE_FORWARD_IN`, expand depending on reverse lookup.

**`VertexRef` remote bit encoding:** unchanged.

See [0005](0005-vertex-identity.md) for **`EncodedVertexId`** / **`EncodedEdgeId`** (client wire).

---

### 4. Client-facing element ids (summary)

Internal components use **`GlobalVertexId`** / **`GlobalEdgeId`**. Clients receive **bijective
encoded opaque bytes**:

| Type | Canonical | Wire (`ELEMENT_ID`, paths) | `Storable` |
|------|-----------|----------------------------|------------|
| Vertex | `GlobalVertexId` (8 B) | `EncodedVertexId` (8 B) | canonical only |
| Edge | `GlobalEdgeId` (12 B) | `EncodedEdgeId` (12 B) | canonical only |

- Deterministic per-graph encoding key (router stable).
- Obfuscates insertion order; reversible for client round-trip; not a security boundary.
- GQL / execution: `Value::Bytes`; Candid: `vec nat8` via `IcWireValue::Bytes`.
- Optional SDK hex/base64 **presentation** only.

`GlobalEdgeId` = `{ shard_id, owner_local, edge_slot_index }` — query-time physical CSR handle,
not stable across compaction.

---

### 5. Index canister grouping

**No index canister numeric id.** Index instances are identified by **`Principal`** only.

**Grouping:** one index canister per **shard group**. Group index is derived:

```text
group_index = shard_id / GROUP_SIZE    // GROUP_SIZE fixed later (capacity-driven)
```

Router resolves `group_index → index_canister Principal` at shard registration (or from a
router-held table). **Graph shard** stores only **its** `index_canister` principal (bootstrap /
`FederationRouting` / registry row) — not the formula, not peer indexes.

**Single shard:** `shard_id = 0`, one group, one index canister.

Index postings remain tagged with **`shard_id`** so one index canister can serve multiple graph
shards in a group.

---

### 6. Stable memory: remove, defer, repack

**Policy:** remove federation machinery that single-shard does not need; defer what federation
will reintroduce behind a later ADR; **pack** remaining `MemoryId`s into consecutive layouts (dev
migration acceptable — no production data).

#### Graph — remove from single-shard layout

| MemoryId | Symbol | Rationale |
|--------|--------|-----------|
| 36 | `VERTEX_LOGICAL_IDS` | Replaced by `GlobalVertexId` derivation |
| 37–38 | `REMOTE_REF_TO_LOGICAL` / `LOGICAL_TO_REMOTE_REF` | Wrong target; deferred remote model |
| 39 | `REMOTE_FORWARD_IN` | Federation-only derived index |
| 41 | `PEER_GRAPH_CANISTERS` | Graph must not resolve peer principals yet |

#### Graph — remove catalog (after router SSOT migration)

| MemoryId | Symbol | Rationale |
|--------|--------|-----------|
| 25–26 | `PROPERTY_NAME_TO_ID` / `PROPERTY_ID_TO_NAME` | Router owns names |

#### Router — remove / simplify

| MemoryId | Symbol | Action |
|--------|--------|--------|
| 5 | `ROUTER_LOGICAL_COUNTER` | Remove |
| 6 | `ROUTER_PENDING_LOGICAL` | Remove |
| 4 | `ROUTER_PLACEMENTS` | Rekey by `GlobalVertexId` / `PhysicalPlacementKey` only |
| 13 | `ROUTER_PLACEMENT_BY_PHYSICAL` | Remove if placement is keyed by physical key only |

#### Repack

- Eliminate **gaps** in graph facade ids (e.g. 30–31, 34–35) and router reserved **14** in one
  layout change per canister.
- **LARA bundle** MemoryIds (0–21 region) are a **separate** repack from facade federation
  leftovers — do not mix piecemeal.
- Update [`stable-memory-inventory.md`](../storage/stable-memory-inventory.md) in the same patch
  as code.

#### Keep for standalone / single shard

- Router `ROUTER_SHARDS` + simplified placement
- Graph `GRAPH_METADATA` (`shard_id`, router, index principals)
- Index postings with `shard_id = 0`
- Index DML flush / backfill
- CSR / LARA core (including remote **bit** in `VertexRef`, without writing remote edges)

---

### 7. Standalone semantics (revised)

Standalone is **`n = 1` shard**, `ShardId(0)`, one index canister, router-owned catalogs, graph
values by id only.

| Concept | Behavior |
|---------|----------|
| Shard | `ShardId(0)` only |
| Global vertex key | `GlobalVertexId(0, local)` |
| Client `ELEMENT_ID` | `EncodedVertexId` / `EncodedEdgeId` |
| Index reads (query) | Router seeds + `lookup_*` on index; no graph index client on wasm federated path |
| Multi-shard dispatch | PocketIC experiments only; not product-supported |
| Remote CSR / expand | Out of scope until remote ref table ADR |

---

### 8. Documentation sync (required with implementation)

| Document | Update |
|----------|--------|
| `design/federation/model.md` | Replace logical-id model; point to 0005/0006 |
| `design/sharding/standalone-mode.md` | Shard 0, catalog SSOT, defer list matches inventory |
| `design/storage/stable-memory-inventory.md` | Repacked ids; removed regions |
| `design/index/property-index.md` | Router `property_id`; posting vertex `(shard_id, local)` |
| `design/glossary.md` | `GlobalVertexId`, `EncodedVertexId`, `RemoteVertexId` |

---

## Consequences

### Positive

- Numeric shard identity without Principals in postings or hot paths.
- Single catalog allocator — index and planner agree on `property_id`.
- Global vertex key matches index postings and placement.
- Smaller single-shard stable footprint; clearer defer boundary for federation.
- Consecutive `MemoryId` layout reduces operational confusion.

### Negative / cost

- Coordinated refactor: `graph-kernel`, router, graph, graph-index, gql-ic, PocketIC harness.
- Breaking client wire: encoded 8/12-byte element ids (see 0005).
- Breaking stable layout: dev reinstall / migration required.
- Remote federation features explicitly blocked until follow-up ADR.

---

## Implementation order (suggested)

1. **`ShardId(u32)` newtype** + harness shards `0`/`1` + register sole shard as `0`.
2. **Catalog ADR implementation** — router SSOT; remove graph property name catalog; wire ids on DML/plan.
3. **Vertex identity** — `GlobalVertexId`, strip logical placement stable, encoded wire ids ([0005](0005-vertex-identity.md)).
4. **Stable memory repack** — remove 36–41, 25–26; repack ids; update inventory.
5. **Doc sync** — standalone, federation model, property-index.
6. **Deferred** — `RemoteVertexId` table, index `GROUP_SIZE` constant, peer principals.

---

## Alternatives considered

| Alternative | Why rejected |
|-------------|--------------|
| Keep `LogicalVertexId` as global key | Redundant surrogate; wrong remote resolution target |
| Graph retains property name catalog | Dual allocator breaks index routing |
| Assign index canister numeric ids | Unnecessary; group derived from `shard_id / GROUP_SIZE` |
| Keep 16-byte edge wire ids | Wastes 4 bytes; GQL uses variable opaque bytes |
| `ShardId` sentinel (e.g. `0` = invalid) | Conflicts with shard `0` being the sole shard |
| Defer all federation types including `ShardId` | Postings and dispatch already embed `shard_id` |

---

## References

- [0005 — Vertex and edge identity (encoded wire ids)](0005-vertex-identity.md)
- [stable-memory-inventory.md](../storage/stable-memory-inventory.md)
- [standalone-mode.md](../sharding/standalone-mode.md)
- [refactoring-roadmap.md](../architecture/refactoring-roadmap.md)
- `crates/graph-kernel/src/federation.rs`
- `crates/router/src/facade/stable/memory.rs`
- `crates/graph/src/facade/stable/memory.rs`
