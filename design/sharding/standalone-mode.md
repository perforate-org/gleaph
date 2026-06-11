# Standalone shard mode

## Status

**Partially implemented** — `ShardId(0)`, `GlobalVertexId`, router catalog SSOT, and encoded element ids are in code (ADR 0005/0006). Federation remote-ref stable, peer ACL, and `federated_expand` were **removed**; cross-shard expand is deferred.

## Purpose

Define the **default execution mode** while multi-shard production rollout is premature: one graph shard, index postings tagged with a fixed local `shard_id`, and no cross-shard orchestration on the query hot path.

## Non-goals

- Multi-shard router fan-out (see [federation-target.md](federation-target.md)).
- Vertex migration or placement state machines beyond minimal hooks.
- Removing `ShardId` / `GlobalVertexId` from `graph-kernel` wire types.

## Standalone semantics

| Concept | Standalone behavior |
|---------|---------------------|
| `ShardId` | `ShardId(0)` — sole shard under strategy A (`0..n-1`); see [ADR 0006](../adr/0006-pre-federation-foundation.md) |
| `GlobalVertexId` | `GlobalVertexId { shard_id: 0, local_vertex_id }` — derived from shard routing + local dense id |
| Client `ELEMENT_ID` | `EncodedVertexId` (8B) / `EncodedEdgeId` (12B) via router encoding key |
| `PlanBinding` | `Vertex(VertexId)` only on the query hot path |
| Index lookup | Router or graph calls index canister; hits filtered to `shard_id == local` |
| Router dispatch | Single shard in registry; no seed required |
| Cross-shard expand | Not used (`Unsupported` or code paths deferred) |

Standalone is the **degenerate case** of the target federation model in [federation-target.md](federation-target.md).

## Code cohesion (planned layout)

Federation-related behavior must not spread across executor, facade stable, and index modules. Target module boundaries:

```text
crates/graph/src/federation.rs
  index_bind.rs    PostingHit → local Vertex binding (no tombstone read filter)
  expand.rs        peer expand coordinator boundary
  routing.rs       local shard id from store routing

crates/router/src/federation.rs
  standalone.rs    single-shard dispatch
  dispatch.rs      multi-shard fan-out (planned target path)
```

**Rule:** executor, scan, and expand code call `FederationPort` only — not direct `placement::resolve_placement` from query hot paths, not direct index intersection loops.

## What stays (hooks)

Keep in `graph-kernel` and wire formats without behavioral commitment:

- `ShardId`, `GlobalVertexId`, `EncodedVertexId` / `EncodedEdgeId`
- `PostingHit { shard_id, vertex_id }`
- `ExecutePlanArgs { target_shard_id, seed_bindings_blob, ... }`
- `ShardRegistryEntry { graph_canister, index_canister, ... }`

## What to defer or remove

Defer detailed implementation and **federation-only stable stores** until [federation-target.md](federation-target.md) is implemented behind an explicit feature or ADR.

| Area | Paths (representative) | Action |
|------|------------------------|--------|
| Remote edge stable | `REMOTE_VERTEX_REFS`, `REMOTE_FORWARD_IN`, `PEER_GRAPH_CANISTERS` | **Removed** |
| Graph placement client | `graph/src/index/placement.rs` IC calls | Defer (keep native test stubs if needed) |
| Graph → index scan on federated wire path | wasm `execute_plan_query` index client | **Removed** — router seeds + skip anchor |
| Executor `RemoteVertex` bind from index | `materialize_federated_index_hits` | **Removed** — `FederationPort` local hits only |
| Router multi-shard without anchor | `gql.rs` error path | Rebuild with `lookup_intersection` seeds |
| Peer graph ACL / `federated_expand` | graph canister endpoints, router `peer_sync` | **Removed** (no-op stubs until follow-up ADR) |

Enum variants such as `PlanBinding::RemoteVertex` may remain for wire compatibility; standalone code paths must not reach them.

## Index invariants (standalone)

The property index is a **projection of live graph property state**:

1. DML inserts/updates/deletes enqueue `posting_insert` / `posting_remove` on the index canister (`graph/src/index/pending.rs`).
2. Vertex delete clears properties first, then removes CSR row — postings for deleted values must not remain in index.
3. Index read APIs do not interpret graph tombstones; stale postings indicate a **DML/index sync bug**, not a query-time filter requirement.

See [lookup-intersection.md](../index/lookup-intersection.md).

## Implementation phases

1. **Index API** — `lookup_intersection` on graph-index; graph executor single-call path (**Implemented**).
2. **Federation module** — `StandaloneFederation`, `FederationPort`, inject into `ExecuteCtx` (**Implemented**).
3. **Router standalone dispatch** — consolidate `gql.rs` dispatch into `router/federation/standalone.rs` (**Implemented**).
4. **Router intersection seeds** — `IndexAnchor`, `lookup_intersection`, graph skip leading `IndexIntersection` (**Implemented**).
5. **Defer removal** — legacy `materialize_federated_index_hits`, federated wire index client (**Implemented**). Remote stable / placement IC defer remain.
6. **Federation target** — router merge module (count + row-batch union + aggregate merge), graph `FederationPort` index bind (**Implemented** on wire path). Peer expand, `federated_expand`, and expand trigger via placement for one-hop / var_len `{1,1}` / shortest-path entry are **not implemented** (deferred). Multi-hop federated var_len BFS and client row API remain planned ([federation-target.md](federation-target.md)).

## Related documents

- [federation-target.md](federation-target.md)
- [../index/lookup-intersection.md](../index/lookup-intersection.md)
- [../federation/query-semantics.md](../federation/query-semantics.md)
