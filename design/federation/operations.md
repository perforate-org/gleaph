# Federation operations

Last updated: 2026-06-18

## Purpose

Document **operational flows**: shard registration, vertex lifecycle, and cross-shard expand. Clarify what is automated vs manual today.

## Non-goals

- Full router admin API reference (generate from Candid when needed).
- Vertex migration playbooks; migration is future work.

## Shard registration

**Implemented**

1. Register shard in router (`ShardRegistryEntry`: graph + index principals).
2. Configure graph shard metadata: `FederationRouting { router_canister, shard_id, index_canister }` (`crates/graph/src/facade/stable/metadata.rs`).

**Invariant (anonymous principal):** `FederationRouting` whose `router_canister` or `index_canister` is `Principal::anonymous()` is rejected at `GraphMetadata::validate_for_store` (the persistence boundary shared by install-time `GraphInitArgs` and `set_federation_routing`). The graph-index canister likewise rejects an anonymous `router_canister` in `IndexStore::init_from_args` before clearing/writing any stable state. Anonymous is never a trusted federation identity; see [security/rbac-and-prepared.md](../security/rbac-and-prepared.md#anonymous-principal-invariant).

There is no post-install graph wiring endpoint: graph shards (including PocketIC fixtures) receive `router_canister`/`shard_id`/`index_canister` only through validated install-time `GraphInitArgs`. A graph never accepts caller-asserted routing after install.

**Removed:** peer graph ACL stable and `bootstrap_graph_peers` / `federated_expand` canister endpoints. Router `peer_sync` is a no-op until cross-shard expand returns in a follow-up ADR.

**Router** resolves **effective graph** from the GQL program (session graph, `HOME_GRAPH` / `is_home`, sole-graph default) → shard list for dispatch ([ADR 0011](../adr/0011-gql-graph-resolution-and-catalog-scoping.md)). Ad-hoc and prepared **execute** APIs take `(query, params)` only — no Candid `logical_graph_name`.

### Remote USE GRAPH (read path)

**Implemented (2026-06-13):** top-level `USE <graph> { … }` / `USE <graph> MATCH … RETURN …` is **defocused** at router ingress: plan is rebuilt with the focused graph’s stats and dispatched to that graph’s shards without a `PlanOp::UseGraph` wrapper. Nested `USE`, inline-procedure `USE`, unsupported pushdown patterns, and remote DML are rejected.

## Vertex create (federated)

**Implemented** (happy path)

1. Graph shard inserts local vertex (LARA).
2. Graph derives `GlobalVertexId { shard_id, local_vertex_id }` from federation routing.
3. Property/label index postings are enqueued on subsequent DML as today.

**Invariant:** Vertex is live when its CSR row is not tombstoned; index projections follow graph DML
([ADR 0017](../adr/0017-graph-vertex-existence-ssot.md)).

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
