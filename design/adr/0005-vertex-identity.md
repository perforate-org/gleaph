# 0005. Vertex and edge identity: global physical keys and encoded wire ids

Date: 2026-06-11  
Status: accepted  
Last revised: 2026-06-17

> **Placement registry:** Router `ROUTER_PLACEMENTS` and placement APIs were removed in
> [0017](0017-graph-vertex-existence-ssot.md). Vertex existence is authoritative on the graph
> shard (CSR tombstone) with index sync on delete.

Part of the broader pre-federation foundation in [0006](0006-pre-federation-foundation.md)
(shard identity, catalogs, stable layout). This ADR details **vertex/edge identity and encoded
wire ids** only.

## Context

Gleaph’s federation model today uses a router-allocated **`LogicalVertexId` (`u64`)** as the
global vertex key. Graph shards maintain **`VERTEX_LOGICAL_IDS`** (local → logical), router
placement is keyed by logical id with a reverse map from physical location, and path /
`ELEMENT_ID` expose logical ids on the wire. Remote cross-shard edges use a shard-local
**`RemoteRefId`** (30-bit payload in `VertexRef`) mapped to **`LogicalVertexId`** in stable
storage.

This design has several problems:

1. **Redundant surrogate.** The authoritative location of a vertex is already
   `(shard_id, local_vertex_id)`. Router `VertexPlacement` already stores
   `PhysicalVertexLocation`; the logical id adds a second global key with no independent meaning.
2. **Wrong remote resolution target.** Remote refs should resolve to
   `(shard_id, local_vertex_id)` inside the owning shard, not to a global logical id.
3. **Information leakage on the client wire.** Raw `(shard_id, local_vertex_id)` or monotonic
   logical ids reveal insertion order and shard layout. Clients need stable, round-trippable ids
   that look opaque.
4. **Oversized edge path ids.** `GraphPathEdgeId` uses 16 bytes on the wire but only 12 bytes
   carry semantics (bytes 4–7 are zero padding). GQL and Candid already treat path element ids as
   variable-length opaque bytes (`PathElementId`, `Value::Bytes`); there is no need for a
   128-bit numeric type.
5. **Premature federation stable regions.** `VERTEX_LOGICAL_IDS`, remote↔logical maps,
   `REMOTE_FORWARD_IN`, and related tables encode the old model and should not be carried forward.

Related policies are specified in [0006](0006-pre-federation-foundation.md): **`ShardId(u32)`**,
router catalog SSOT, index grouping, stable memory cleanup, `VertexRef` remote bit unchanged.

## Decision

Adopt a **two-layer identity model**: canonical **graph-scoped physical keys** for all internal
components (interpreted only with explicit **`GraphId`** execution context), and **encoded opaque
bytes** for client-visible `ELEMENT_ID` and path elements.

> **Amended 2026-06-17 ([ADR 0019](0019-graph-local-shard-id-and-index-clusters.md)):** **`Global`**
> means **within one logical graph partition**, not unique across the whole router federation.
> `GraphId` is **not embedded** in `GlobalVertexId`, `GlobalEdgeId`, or encoded wire payloads.
> Cross-graph code must thread `GraphId` alongside physical keys. Encoded bytes are
> **graph-context-bound handles** — use `(GraphId, Encoded*)` when graph boundaries may be crossed.

### Identity layers

```text
┌─────────────────────────────────────────────────────────────────┐
│ Internal (router, index, placement, federation APIs)            │
│   GlobalVertexId { shard_id, local_vertex_id }     8 bytes      │
│   GlobalEdgeId   { shard_id, owner_local, slot }  12 bytes      │
│   RemoteVertexId (shard-local only, 30-bit)        never exported│
└─────────────────────────────────────────────────────────────────┘
                              │ encode(key) / decode(key)
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Client wire (ELEMENT_ID, path vertices/edges, GQL Value::Bytes) │
│   EncodedVertexId   [u8; 8]   — no Storable                    │
│   EncodedEdgeId     [u8; 12]  — no Storable                     │
└─────────────────────────────────────────────────────────────────┘
```

### Global vertex identity

- **`GlobalVertexId`** is the single canonical graph-scoped vertex key:
  `{ shard_id: ShardId, local_vertex_id: LocalVertexId }`.
- **`shard_id`** is **graph-local** ([ADR 0019](0019-graph-local-shard-id-and-index-clusters.md)):
  `(ShardId(0), local)` on graph `a` ≠ `(ShardId(0), local)` on graph `b` without matching
  `GraphId` context.
- Replaces **`LogicalVertexId`** and subsumes today’s **`PhysicalPlacementKey`** /
  **`PhysicalVertexLocation`** field pairing (same 8-byte little-endian layout).
- Router placement, index **`PostingHit`**, federation expand arguments, and internal maps use
  **`GlobalVertexId`** only.
- Router records active vertices by physical key; no logical id allocation, no pending logical
  counter, no physical → logical reverse map.

### Global edge identity (query-time)

- **`GlobalEdgeId`** identifies an edge at query time:
  `{ shard_id, owner_vertex_id: LocalVertexId, edge_slot_index: EdgeSlotIndex }` (12 bytes).
- This is a **physical CSR handle**, not a stable logical edge id across compaction — same
  limitation as today’s `GraphPathEdgeId`.
- There is no global “logical edge id” in this ADR.

### Encoded wire ids (client-facing)

- **`EncodedVertexId`**: 8-byte opaque, bijectively encoded from **`GlobalVertexId`**.
- **`EncodedEdgeId`**: 12-byte opaque, bijectively encoded from **`GlobalEdgeId`** (not 16 bytes).
- Encoding uses a **fixed-key bijection** (e.g. Feistel rounds) with a **per-graph key** held by
  the router in `ROUTER_GRAPH_RUNTIME_CONFIG` (derived at graph registration; production uses IC
  `raw_rand()` — [ADR 0019](0019-graph-local-shard-id-and-index-clusters.md) §3.1).
- Properties:
  - **Deterministic** — same canonical id always encodes to the same bytes.
  - **Bijective** — client-sent bytes decode back to exactly one canonical id.
  - **Obfuscating** — hides insertion order and shard layout at a glance.
  - **Not a security boundary** — does not prevent inference by a motivated observer with many
    samples.
- **`EncodedVertexId`** and **`EncodedEdgeId`** are **wire-only** types: they do **not** implement
  **`Storable`** and must not appear in stable storage or index posting keys.
- **`GraphPathVertexId`** / **`GraphPathEdgeId`** wrap the encoded types for path semantics.

Constants:

```text
ENCODED_VERTEX_ID_BYTES = 8
ENCODED_EDGE_ID_BYTES   = 12
GLOBAL_VERTEX_ID_BYTES  = 8
GLOBAL_EDGE_ID_BYTES    = 12
```

### Remote vertex handles (shard-internal)

- Rename **`RemoteRefId`** → **`RemoteVertexId`** (30-bit payload; `VertexRef` remote bit
  unchanged).
- **`RemoteVertexId` never leaves the graph shard** — not on router, index, or client APIs.
- Each shard will eventually maintain a persistent mutual index
  **`RemoteVertexId ↔ GlobalVertexId`** — **deferred**; not implemented in the initial migration.
- Remote CSR edge creation and expand that depend on reverse lookup remain **out of scope** until
  that table exists.

**Allocator policy for `RemoteVertexId`:**

- Valid assigned range: **`[1, 2^30 − 1]`**.
- **`0` is never issued** — not because the type needs a public `INVALID` sentinel, but as
  allocator hygiene (all-zero detection, separation from tombstones).
- Absence uses **`Option<RemoteVertexId>`** at API and table boundaries.
- Deleted CSR slots continue to use **`VertexRef::tombstone()`** (bit 31), not remote id 0.
- Drop **`RemoteRefId::INVALID`**, **`is_valid()`**, and **`Default`** on the type.

### Candid and SDK presentation

- Internal execution and **`gleaph-gql`** use **`Value::Bytes`** with fixed 8- or 12-byte payloads.
- **`gql-ic`** maps these to Candid **`vec nat8`** via **`IcWireValue::Bytes`** — lossless, compact.
- Candid has no 128-bit integer type; this is irrelevant because ids are never Candid numerics.
- Optional SDK helpers may present ids as hex or base64url **strings** for ergonomics; that is a
  presentation layer only. Decode must recover the exact byte sequence before **`decode()`** into
  canonical types.

### Removal of the logical-id model

**Remove (or do not reintroduce):**

| Artifact | Action |
|----------|--------|
| `LogicalVertexId` | Remove type and all APIs |
| `VERTEX_LOGICAL_IDS` (graph stable) | Remove |
| `REMOTE_REF_TO_LOGICAL` / `LOGICAL_TO_REMOTE_REF` | Remove |
| `REMOTE_FORWARD_IN` | Remove until remote model is reimplemented |
| `ROUTER_LOGICAL_COUNTER`, `ROUTER_PENDING_LOGICAL` | Remove |
| `ROUTER_PLACEMENTS` keyed by logical id | Replace with placement keyed by `GlobalVertexId` |
| `allocate_logical_vertex_id`, `resolve_placement(logical)` | Replace with physical-key APIs |
| `standalone_logical_vertex_id` | Remove |

**Defer:**

- Persistent **`RemoteVertexId ↔ GlobalVertexId`** mutual index per shard.
- Remote edge DML and federated expand depending on that index.
- Vertex migration (placement transition states).

### Router placement (simplified)

```text
INSERT  → graph allocates local_vertex_id on shard
        → graph calls commit_vertex_placement { local_vertex_id }
        → router records GlobalVertexId(shard_id, local) as active

QUERY   → materialize ELEMENT_ID = encode(GlobalVertexId)
CLIENT  → sends EncodedVertexId bytes (or SDK-decoded bytes)
        → decode → GlobalVertexId → router resolve / shard dispatch
```

## Consequences

### Positive

- One global vertex key aligned with index postings and physical storage.
- Remote handles stay shard-local; router and clients never see `RemoteVertexId`.
- Client ids are stable and round-trippable without revealing monotonic local allocation.
- 12-byte edge ids save wire space and match information content.
- Clear separation: **`Storable`** canonical types vs non-persistent encoded wire types.

### Negative / migration

- **Breaking wire change** for `ELEMENT_ID` and path element bytes (logical `u64` / 16-byte edge
  layout → encoded 8 / 12 bytes). Acceptable in pre-production; update tests and docs.
- Coordinated refactor across router, graph, graph-kernel, path materialization, eval, PocketIC
  harness.
- Federation stable MemoryIds 36–41 can be dropped and ids repacked (dev migration OK).
- `design/federation/model.md` identity section is superseded by this ADR.

### Implementation status

| Item | Status |
|------|--------|
| ADR and type definitions in `graph-kernel` | **Implemented** |
| Remove `LogicalVertexId` and logical stable regions | **Implemented** |
| `GlobalVertexId` router placement | **Implemented** |
| `EncodedVertexId` / `EncodedEdgeId` encode-decode | **Implemented** |
| `RemoteVertexId` rename + allocate-from-1 policy | **Implemented** (type rename; no allocator/stable yet) |
| Persistent remote vertex ↔ global index | **Deferred** |
| Remote edge DML / expand | **Deferred** |

## Alternatives considered

### Keep `LogicalVertexId` as the global key

Rejected. It duplicates `(shard_id, local_vertex_id)`, forces extra stable maps and router
allocation, and maps remote refs to the wrong abstraction.

### Expose raw `(shard_id, local_vertex_id)` to clients

Rejected. Reveals insertion order and shard layout; poor UX for opaque `ELEMENT_ID`.

### One-way hash for client ids

Rejected. Clients must send ids back; encoding must be **bijective**, not hashed.

### 16-byte `EncodedEdgeId` (pad to `u128`)

Rejected. Wastes 4 bytes; no Candid or GQL requirement for 16-byte edge ids. Canonical edge
identity is 12 bytes; encoded form matches.

### `RemoteVertexId::INVALID` sentinel (0)

Rejected. Tombstones and `Option` cover absence; allocator starts at 1 without a public invalid
constant.

### Candid `Text` as the only wire representation

Rejected as canonical form. `vec nat8` is smaller and already implemented; string encoding is
optional SDK presentation.

## References

- `crates/graph-kernel/src/federation.rs` — current placement types (to be updated)
- `crates/graph-kernel/src/path.rs` — current path id layout (to be updated)
- `crates/graph-kernel/src/entry/remote_vertex_id.rs` — `RemoteVertexId` / `VertexRef` remote payload
- `crates/gql-ic/src/wire.rs` — Candid mapping for `Value::Bytes`
- `design/federation/model.md` — prior identity model (to be revised)
- `design/storage/stable-memory-inventory.md` — MemoryIds 36–41 (to be revised)
