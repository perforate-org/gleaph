# 0017. Vertex existence SSOT on graph shard; remove router placement registry

Date: 2026-06-17
Status: accepted
Last revised: 2026-06-17

Supersedes the router placement authority described in [0005](0005-vertex-identity.md) (placement
section) and [0006](0006-pre-federation-foundation.md) (router `commit_vertex_placement`).

## Context

Gleaph uses `GlobalVertexId { shard_id, local_vertex_id }` as the canonical global vertex key
(ADR 0005). Router stable `ROUTER_PLACEMENTS` recorded `VertexPlacement::Active` on graph DML
commit/release, and graph shards called `resolve_placement` before federated expand and some DML
paths.

This duplicated existence truth:

1. **Graph CSR** already records liveness via the vertex-row tombstone bit; `VertexId` is not
   reused after delete.
2. **Property and label indexes** are projections maintained on graph DML; vertex delete clears
   sidecars and enqueues index removals before the CSR row is tombstoned.
3. **Router query dispatch** never read `ROUTER_PLACEMENTS`; only graph clients and tests did.
   Wasm production paths already stubbed placement reads and skipped release on delete.

Maintaining a second registry on the router added inter-canister calls on insert/delete, allowed
router/graph drift (Active placement + tombstoned vertex), and did not help index seeding or
shard routing (`ROUTER_SHARDS` / `ROUTER_SHARD_BY_GRAPH` remain the federation dispatch SSOT).

## Decision

**Vertex and edge existence for query and DML is authoritative on the graph shard** (CSR
tombstone + sidecar/index sync). **Remove router placement registry and APIs.**

### Removed

| Artifact | Action |
|----------|--------|
| `ROUTER_PLACEMENTS` (router MemoryId 4) | Remove — slot reclaimed in 2026-06-17 category repack ([ADR 0007](0007-stable-memory-layout.md)) |
| `commit_vertex_placement`, `release_vertex_placement` | Remove router update endpoints |
| `resolve_placement`, `resolve_global_at` | Remove router query endpoints |
| `VertexPlacement`, `CommitVertexPlacementArgs`, `ReleaseVertexPlacementArgs` | Remove from `graph-kernel` wire types |
| `PhysicalVertexLocation` | Remove (same information as `GlobalVertexId`) |
| Graph `placement::commit/release/resolve` IC client | Remove |
| `RouterError::{VertexNotFound, PlacementAlreadyCommitted, UnallocatedLogicalVertex}` | Remove placement-only variants |

### Authoritative existence (graph shard)

| Question | SSOT | Mechanism |
|----------|------|-----------|
| Is local vertex live? | Graph CSR | Row exists in range and `!vertex.is_tombstone()` |
| Is `GlobalVertexId` live on this shard? | Graph | `shard_id` matches routing + local liveness check |
| Index scan hit valid? | Graph DML → index sync | Postings removed on delete; stale hits = sync bug |
| Which canister owns a shard? | Router registry | `resolve_shard` / `ROUTER_SHARDS` (unchanged) |
| Client wire id | Router encoding key | `EncodedVertexId` ↔ `GlobalVertexId` bijection (unchanged) |

### Graph helpers (implemented)

- `GraphStore::is_vertex_live(VertexId)` — tombstone-aware liveness on the local shard.
- `GraphStore::resolve_local_vertex(GlobalVertexId) -> Option<VertexId>` — home-shard live local
  handle, if any.

Federated expand and traversal use these helpers instead of router placement calls. Cross-shard
expand remains deferred; foreign `shard_id` still returns `UnsupportedOp`.

### Index invariants (unchanged policy, clarified owner)

Property and label postings are **derived projections** of live graph state. Index read APIs do
not interpret tombstones; delete DML must enqueue removals before tombstoning the CSR row.

## Consequences

### Positive

- Single existence SSOT aligned with storage and index maintenance.
- No placement inter-canister traffic on vertex insert/delete.
- Router stable footprint reduced; no placement/graph drift class of bugs.
- Matches wasm behavior already used on query hot paths.

### Negative / migration

- **Breaking Candid change:** placement endpoints and error variants removed from router.
- **Stable layout repack:** router MemoryIds regrouped by category (auth → maintenance); placement region removed ([ADR 0007](0007-stable-memory-layout.md)).
- External tools that called `resolve_placement` must query the owning graph shard or use GQL
  `ELEMENT_ID` materialization instead.
- Future vertex migration must tombstone the source row on the graph shard (same as today’s delete
  semantics); no router placement transition state.

## Alternatives considered

1. **Keep router registry as cache** — rejected; graph tombstone + index sync already define live
   state; cache adds drift without helping router dispatch.
2. **Router verifies existence on every encoded-id dispatch** — rejected; router does not read
   graph CSR; would require inter-canister calls on the query hot path.
3. **Infer live from index postings alone** — rejected; index is derived and may lag; tombstone on
   graph is canonical.

## Implementation status

| Item | Status |
|------|--------|
| ADR 0017 | **Accepted** |
| Remove router placement stable region and APIs | **Implemented** |
| Repack router MemoryIds by category (auth → maintenance) | **Implemented** |
| Graph liveness helpers + call-site migration | **Implemented** |
| Design doc sync (`model.md`, inventory, ADR 0005/0006 notes) | **Implemented** |
