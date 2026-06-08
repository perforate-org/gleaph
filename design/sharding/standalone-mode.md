# Standalone shard mode

## Status

**Planned** — engineering direction. Wire hooks (`ShardId`, `PostingHit`, `ExecutePlanArgs`) remain in code; detailed federation runtime and dedicated stable stores are deferred.

## Purpose

Define the **default execution mode** while multi-shard production rollout is premature: one graph shard, index postings tagged with a fixed local `shard_id`, and no cross-shard orchestration on the query hot path.

## Non-goals

- Multi-shard router fan-out (see [federation-target.md](federation-target.md)).
- Vertex migration or placement state machines beyond minimal hooks.
- Removing `ShardId` / `LogicalVertexId` from `graph-kernel` wire types.

## Standalone semantics

| Concept | Standalone behavior |
|---------|---------------------|
| `ShardId` | Fixed local id (typically `0`) on all postings and dispatch |
| `LogicalVertexId` | `standalone_logical_vertex_id(local)` — local dense id equals logical id |
| `PlanBinding` | `Vertex(VertexId)` only on the query hot path |
| Index lookup | Router or graph calls index canister; hits filtered to `shard_id == local` |
| Router dispatch | Single shard in registry; no seed required |
| Cross-shard expand | Not used (`Unsupported` or code paths deferred) |

Standalone is the **degenerate case** of the target federation model in [federation-target.md](federation-target.md).

## Code cohesion (planned layout)

Federation-related behavior must not spread across executor, facade stable, and index modules. Target module boundaries:

```text
crates/graph/src/federation/
  mod.rs           FederationPort trait + StandaloneFederation
  index_bind.rs    PostingHit → local Vertex binding (no tombstone read filter)
  expand.rs        peer expand (stub / Unsupported in standalone)

crates/router/src/federation/
  mod.rs           ShardingPolicy trait
  standalone.rs    single-shard dispatch
  dispatch.rs      multi-shard fan-out (planned)
  seed.rs          index anchor → per-shard seeds (planned)
```

**Rule:** executor, scan, and expand code call `FederationPort` only — not `placement::resolve_logical_at`, not direct index intersection loops.

## What stays (hooks)

Keep in `graph-kernel` and wire formats without behavioral commitment:

- `ShardId`, `LogicalVertexId`, `PhysicalPlacementKey`
- `PostingHit { shard_id, vertex_id }`
- `ExecutePlanArgs { target_shard_id, seed_bindings_blob, ... }`
- `ShardRegistryEntry { graph_canister, index_canister, ... }`

## What to defer or remove

Defer detailed implementation and **federation-only stable stores** until [federation-target.md](federation-target.md) is implemented behind an explicit feature or ADR.

| Area | Paths (representative) | Action |
|------|------------------------|--------|
| Remote edge stable | `graph/.../remote_forward_in.rs`, `remote_vertex_refs.rs` | Defer |
| Graph placement client | `graph/src/index/placement.rs` IC calls | Defer (keep native test stubs if needed) |
| Graph → index scan on executor | `execute_index_scan`, `execute_index_intersection` calling index directly | Replace with router-owned lookup + seeds (target) |
| Executor `RemoteVertex` bind from index | `scan/index.rs` `materialize_federated_index_hits` | Defer |
| Router multi-shard without anchor | `gql.rs` error path only partially useful | Rebuild with `lookup_intersection` seeds |
| Peer graph ACL stable | `peer_graph_canisters.rs` | Defer |

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
3. **Router standalone dispatch** — consolidate `gql.rs` dispatch into `router/federation/standalone.rs` (Planned).
4. **Defer removal** — gate or delete immature federation stable/runtime (Planned).
5. **Federation target** — router index slice, peer expand, merge ([federation-target.md](federation-target.md)) (Planned).

## Related documents

- [federation-target.md](federation-target.md)
- [../index/lookup-intersection.md](../index/lookup-intersection.md)
- [../federation/query-semantics.md](../federation/query-semantics.md)
