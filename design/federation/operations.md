# Federation operations

Last updated: 2026-06-13

## Purpose

Document **operational flows**: shard registration, vertex lifecycle, and cross-shard expand. Clarify what is automated vs manual today.

## Non-goals

- Full router admin API reference (generate from Candid when needed).
- Vertex migration playbooks; migration is future work.

## Shard registration

**Implemented**

1. Register shard in router (`ShardRegistryEntry`: graph + index principals).
2. Configure graph shard metadata: `FederationRouting { router_canister, shard_id, index_canister }` (`crates/graph/src/facade/stable/metadata.rs`).

**Removed:** peer graph ACL stable and `bootstrap_graph_peers` / `federated_expand` canister endpoints. Router `peer_sync` is a no-op until cross-shard expand returns in a follow-up ADR.

**Router** resolves **effective graph** from the GQL program (session graph, HOME, sole-graph default) → shard list for dispatch ([ADR 0011](../adr/0011-gql-graph-resolution-and-catalog-scoping.md)).

**Legacy (today):** query APIs still pass `logical_graph_name` as a Candid argument; this bypasses `SESSION SET GRAPH` and will be removed.

## Vertex create (federated)

**Implemented** (happy path)

1. Graph shard inserts local vertex (LARA).
2. Graph derives `GlobalVertexId { shard_id, local_vertex_id }` from federation routing.
3. Graph calls router `commit_vertex_placement` with `{ local_vertex_id }`.
4. Placement becomes `VertexPlacement::Active`.

**Invariant:** No federated reads should treat the vertex as globally visible until placement is committed (exact visibility rules follow router index updates).

## Federated expand

**Not implemented** — `federated_expand`, remote-vertex stable, and peer ACL were removed. Cross-shard traverse returns `UnsupportedOp` until [federation-target.md](../sharding/federation-target.md) is implemented behind a follow-up ADR.

Target semantics (when implemented):

| Direction | Semantics |
|-----------|-----------|
| **Outgoing** (from authoritative shard) | Traverse real CSR out-edges; neighbors may be local or remote `RemoteVertexId` endpoints. |
| **Incoming** (fan-out) | Coordinator queries **all** shards for edges pointing at a `GlobalVertexId`. |

## Migration

Future work — no operational runbook yet. Adding migration requires an explicit placement transition state and runtime protocol together with router and graph changes.

## Related documents

- [model.md](model.md)
- [query-semantics.md](query-semantics.md)
- [../sharding/federation-target.md](../sharding/federation-target.md)
- [../sharding/standalone-mode.md](../sharding/standalone-mode.md)
