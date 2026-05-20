# Federation operations

## Purpose

Document **operational flows**: shard registration, vertex lifecycle, cross-shard expand, and migration. Clarify what is automated vs manual today.

## Non-goals

- Full router admin API reference (generate from Candid when needed).
- Disaster recovery playbooks (future `migration-playbook.md`).

## Shard registration

**Implemented**

1. Register shard in router (`ShardRegistryEntry`: graph + index principals).
2. Configure graph shard metadata: `FederationRouting { router_canister, shard_id, index_canister }` (`crates/graph/src/facade/stable/metadata.rs`).
3. Peer graph ACL sync (`peer_sync.rs`) so shards may call `federated_expand` on siblings.

**Router** resolves `logical_graph_name` → shard list for dispatch.

## Vertex create (federated)

**Implemented** (happy path)

1. Graph shard inserts local vertex (LARA).
2. Router allocates `LogicalVertexId` (or caller supplies per API contract).
3. Graph calls router `commit_vertex_placement` with `CommitVertexPlacementArgs { logical_vertex_id, local_vertex_id }`.
4. Placement becomes `VertexPlacement::Active`.

**Invariant:** No federated reads should treat the vertex as globally visible until placement is committed (exact visibility rules follow router index updates).

## Federated expand

**Implemented** — `federated_expand` canister API and `federation_expand_coordinator` (`crates/graph/src/facade/federation_expand.rs`).

| Direction | Semantics |
|-----------|-----------|
| **Outgoing** (from authoritative shard) | Traverse real CSR out-edges; neighbors may be local or remote refs. |
| **Incoming** (fan-out) | Coordinator queries **all** shards; each returns matches for edges pointing at the logical target. |

Uses:

- Authoritative shard: directed in-edges on local copy.
- Non-authoritative: `REMOTE_FORWARD_IN` index, with scan+backfill fallback when index cold.

**Limit:** Inter-canister path requires **wasm** (`UnsupportedOp` on native builds in `crates/graph/src/index/federation.rs`).

## Migration (data plane)

**Implemented** on graph shard (primitives + tests in `crates/graph/src/facade/migration/vertex.rs`):

| Step | API / action | Shard |
|------|----------------|-------|
| 1 | Router `begin_vertex_migration` | Router sets `Migrating` |
| 2 | `migration_export` bundle | Source (read-only export) |
| 3 | `migration_import` | Destination |
| 4 | Router `finish_vertex_migration` | Destination local id bound |
| 5 | `migration_tombstone` + `release_logical_vertex` | Source cleanup |

**Not implemented (orchestration gap):** Router does **not** automatically chain export → import → tombstone. Operators or future router workflows must drive steps explicitly.

### Migration invariants

- Source writes blocked while `Migrating` on source.
- Destination must not serve authoritative reads until finish completes (see `VertexPlacement` checks in expand/traversal).

## Property index during federation

Graph-index records `shard_id` per posting. Router uses equality lookup to build multi-shard seed routings (`crates/router/src/gql.rs`).

Index mutations on graph may be dropped if no index client is configured (`index/pending.rs`) — document in [index/property-index.md](../index/property-index.md).

## Failure modes (router)

Representative `RouterError` variants (`federation/router_error.rs`):

- `VertexNotFound`, `VertexMigrating`
- `ShardNotRegistered`, `InvalidArgument`

Graph surfaces placement failures as `GraphStoreError::VertexPlacement` / `VertexMigrating`.

## Related documents

- [model.md](model.md)
- [query-semantics.md](query-semantics.md)
- [architecture/overview.md](../architecture/overview.md)
