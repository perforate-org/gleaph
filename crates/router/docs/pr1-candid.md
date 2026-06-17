# PR1 — `gleaph-router` Candid API

> **As of 2026-06-17 (ADR 0019):** graph-scoped catalog APIs, shard resolution, and index attach
> signatures below reflect the current implementation. PR1 history (placement APIs, etc.) is retained
> with strikethrough where superseded.

Control-plane canister for federated Gleaph graphs. PR1 establishes the router as metadata authority
(graph registry, shard registry, label/property intern) without GQL execution or vertex migration.

**Vertex existence:** authoritative on the **graph shard** (CSR tombstone + index sync), not the router
([ADR 0017](../../../design/adr/0017-graph-vertex-existence-ssot.md)). Placement APIs documented below
were removed; this file retains PR1 history with strikethrough notes where applicable.

**Related decisions (PR1):**

- `GlobalVertexId { shard_id, local_vertex_id }` — canonical global vertex key ([ADR 0005](../../../design/adr/0005-vertex-identity.md)).
- `ShardId = nat32` — partition id; routing SSOT on router (`resolve_shard`, shard registry).
- Index posting keys stay **physical**: `(property_id, encoded_value, shard_id, local_vertex_id)`.
- Shard registry moves from `gleaph-graph-index` to the router; index keeps shard/canister attachment map only.
- Breaking changes are acceptable (no migration from prior federation v1 wire formats).

---

## Shared types

Portable types live in `gleaph-graph-kernel` (`federation`, `index`). Router- and registry-specific
records are defined here for Candid export.

```candid
type ShardId = nat32;
type LocalVertexId = nat32;

// Label and property ids match `gleaph-graph-kernel::entry` (distinct namespaces).
type VertexLabelId = nat16;
type EdgeLabelId = nat16;
type PropertyId = nat32;
// IndexId = nat32 — reserved; not exported in PR1.

type GraphStatus = variant {
  Active;
  ReadOnly;
  Deprecated;
  Deleting;
};

type ProvisioningState = variant {
  None;
  Pending : record { request_id : text };
  Failed : record { request_id : text; reason : text };
};

type GraphRegistryEntry = record {
  graph_name : text;
  owner : principal;
  admins : vec principal;
  status : GraphStatus;
  version : nat64;
  updated_at_ns : nat64;
  provisioning_state : ProvisioningState;
};

type ShardRegistryEntry = record {
  shard_id : ShardId;
  graph_canister : principal;
  index_canister : principal;
  graph_id : nat32;
  registered_at_ns : nat64;
  index_attached : bool;
};

type RouterError = variant {
  NotAuthorized;
  Forbidden;
  NotFound : text;
  Conflict : text;
  InvalidArgument : text;
  GraphUnavailable;
  ShardNotRegistered;
  ShardAlreadyRegistered;
  Internal : text;
};
```

**Removed (ADR 0017):** `LogicalVertexId`, `PhysicalVertexLocation`, `VertexPlacement`,
`VertexNotFound`, `PlacementAlreadyCommitted`, `UnallocatedLogicalVertex`, and placement endpoints
(`resolve_placement`, `allocate_logical_vertex_id`, `commit_vertex_placement`, `release_vertex_placement`).

---

## Init

| Method | Args | Returns | Notes |
|--------|------|---------|-------|
| `init` | `RouterInitArgs` | — | Canister install |

```candid
type RouterInitArgs = record {
  issuing_principal : principal;
  initial_admins : vec principal;
};
```

---

## Query (read)

| Method | Args | Returns | Auth (PR1) | Notes |
|--------|------|---------|------------|-------|
| `whoami` | — | `principal` | public | Same pattern as `gleaph-graph` |
| `resolve_graph` | `graph_name : text` | `Result<GraphRegistryEntry, RouterError>` | caller | Owner or admin only |
| `resolve_shard` | `logical_graph_name : text`, `shard_id : ShardId` | `Result<ShardRegistryEntry, RouterError>` | **public** | Graph-scoped shard resolution |
| `lookup_vertex_label_id` | `logical_graph_name : text`, `name : text` | `Result<VertexLabelId, RouterError>` | public | Graph-scoped catalog |
| `lookup_edge_label_id` | `logical_graph_name : text`, `name : text` | `Result<EdgeLabelId, RouterError>` | public | Graph-scoped catalog |
| `lookup_property_id` | `logical_graph_name : text`, `name : text` | `Result<PropertyId, RouterError>` | public | Graph-scoped catalog |
| `reverse_vertex_label_name` | `logical_graph_name : text`, `label_id : VertexLabelId` | `Result<text, RouterError>` | public | Optional; planner/debug |
| `reverse_edge_label_name` | `logical_graph_name : text`, `label_id : EdgeLabelId` | `Result<text, RouterError>` | public | Optional; planner/debug |
| `reverse_property_name` | `logical_graph_name : text`, `property_id : PropertyId` | `Result<text, RouterError>` | public | Optional; planner/debug |

**Public read APIs:** `resolve_shard` and metadata lookups expose registry directory to any caller that
can reach the canister (same policy as `gleaph-graph-index` lookups today). Gate at a higher layer if
needed.

`resolve_graph_canister` is intentionally omitted; use `resolve_graph` and read `canister_id` from the
entry when graph registry entries include a graph shard principal (future multi-shard graphs may extend
this).

---

## Update — router admin

Caller must have **`Role::Admin`** in stable auth (`ROUTER_AUTH_PRINCIPAL_RECORDS`). Init seeds
`issuing_principal` and `initial_admins` as Admin. Grant or revoke via `admin_grant_role`.

| Method | Args | Returns | Notes |
|--------|------|---------|-------|
| `admin_register_graph` | `entry : GraphRegistryEntry` | `Result<(), RouterError>` | |
| `admin_unregister_graph` | `logical_graph_name : text` | `Result<(), RouterError>` | Fails if shards remain registered |
| `admin_update_graph_status` | `graph_name : text`, `status : GraphStatus`, `version : nat64` | `Result<(), RouterError>` | Optimistic `version` |
| `admin_register_shard` | `AdminRegisterShardArgs` | `Result<(), RouterError>` | Router registry + IC call to index |
| `admin_unregister_shard` | `logical_graph_name : text`, `shard_id : ShardId` | `Result<(), RouterError>` | Graph-scoped detach/remove |
| `admin_intern_vertex_label` | `logical_graph_name : text`, `name : text` | `Result<VertexLabelId, RouterError>` | Idempotent; `0` reserved |
| `admin_intern_edge_label` | `logical_graph_name : text`, `name : text` | `Result<EdgeLabelId, RouterError>` | Idempotent; max `0x7FFF` catalog id |
| `admin_intern_property` | `logical_graph_name : text`, `name : text` | `Result<PropertyId, RouterError>` | Idempotent intern |

```candid
type AdminRegisterShardArgs = record {
  shard_id : ShardId;
  graph_canister : principal;
  index_canister : principal;
  logical_graph_name : text;
};
```

**`admin_register_shard` side effect:** commits router registry row with `index_attached = false`, assigns `index_cluster`, then calls the index canister. On success, sets `index_attached = true`. Dispatch and index lookup use **live** shards only (`index_attached = true`). If attach fails, router rolls back the shard row and reconciles `index_cluster`.

```text
index.admin_attach_shard_canister(graph_id, index_group_size, group_index, shard_id, graph_canister)
```

**`admin_unregister_shard`:** sets `index_attached = false` (excludes shard from read paths), detaches index, then removes router registry row and reconciles `index_cluster`.

---

## Update — graph shard *(removed ADR 0017)*

PR1 allocated logical vertex ids and committed placement on the router. **Removed:** vertex existence
is authoritative on the graph shard; graph shards no longer call router placement endpoints.

---

## PR1 export summary

```
init(RouterInitArgs)

// query
whoami() -> principal
resolve_graph(text) -> Result<GraphRegistryEntry, RouterError>
resolve_shard(text, ShardId) -> Result<ShardRegistryEntry, RouterError>
lookup_vertex_label_id(text, text) -> Result<VertexLabelId, RouterError>
lookup_edge_label_id(text, text) -> Result<EdgeLabelId, RouterError>
lookup_property_id(text, text) -> Result<PropertyId, RouterError>
reverse_vertex_label_name(text, VertexLabelId) -> Result<text, RouterError>   // optional
reverse_edge_label_name(text, EdgeLabelId) -> Result<text, RouterError>       // optional
reverse_property_name(text, PropertyId) -> Result<text, RouterError>          // optional

// update — admin
admin_register_graph(GraphRegistryEntry) -> Result<(), RouterError>
admin_unregister_graph(text) -> Result<(), RouterError>
admin_update_graph_status(text, GraphStatus, nat64) -> Result<(), RouterError>
admin_register_shard(AdminRegisterShardArgs) -> Result<(), RouterError>
admin_unregister_shard(text, ShardId) -> Result<(), RouterError>
admin_intern_vertex_label(text, text) -> Result<VertexLabelId, RouterError>
admin_intern_edge_label(text, text) -> Result<EdgeLabelId, RouterError>
admin_intern_property(text, text) -> Result<PropertyId, RouterError>
```

---

## Not in PR1

| API / feature | Target PR |
|---------------|-----------|
| `gql_query` / `gql_execute` on router | PR3+ |
| `admin_intern_index` | index sharding |
| RemoteRef / remote edges | PR3 |
| Federated query fan-out | PR3 |

---

## Breaking changes in sibling canisters (PR1)

### `gleaph-graph` — `GraphInitArgs`

```candid
type GraphInitArgs = record {
  issuing_principal : principal;
  initial_admins : vec principal;
  logical_graph_name : opt text;
  router_canister : principal;   // required when federated
  shard_id : ShardId;            // required together with router_canister
};
```

Removed: `index_canister`, `graph_shard_id` (index principal comes from `resolve_shard`).

**Init flow (graph):**

1. `resolve_shard(shard_id)` on router.
2. Assert `graph_canister == self` and `logical_graph_name` matches bootstrap name.
3. Cache `index_canister` in stable metadata (`FederationRouting`).

### `gleaph-graph-index`

| Change | Detail |
|--------|--------|
| Remove | `admin_register_shard`, `resolve_shard_principal` |
| Add | `admin_attach_shard_canister(graph_id, index_group_size, group_index, ShardId, principal) -> Result<(), text>` — **router caller only** |
| Change | `posting_*` / `PostingHit.shard_id` → `ShardId` (`nat32`) |
| Change | `PostingKey` encoding — `shard_id` 4 bytes; consider `POSTING_KEY_MAGIC = 2` |

```candid
type IndexInitArgs = record {
  router_canister : principal;
};
```

---

## Type placement (Rust)

| Types | Crate |
|-------|-------|
| `ShardId`, `GlobalVertexId` | `gleaph-graph-kernel::federation` |
| `PostingHit`, `PostingRangeRequest` | `gleaph-graph-kernel::index` |
| `GraphRegistryEntry`, `GraphStatus`, `ProvisioningState` | `gleaph-gql-ic` |
| `VertexLabelId`, `EdgeLabelId`, `PropertyId` | `gleaph-graph-kernel::entry` |
| `ShardRegistryEntry`, `AdminRegisterShardArgs`, `RouterError` | `gleaph-router` / `graph-kernel::federation` |
| `FederationRouting` | `gleaph-graph::facade::stable::metadata` |

See `crates/router/docs/` (module layout) and workspace `graph` / `graph-index` facades for implementation
structure.

---

## Open choices (lock before coding)

| # | Question | Recommendation |
|---|----------|----------------|
| 1 | Separate `allocate` + `commit` vs single call? | **Separate** — local LARA insert between calls |
| 2 | Include `shard_id` in `commit` args? | **No** — derive from caller principal |
| 3 | Export `reverse_*_name` in PR1? | **Yes** — low cost, helps planner wiring |
| 4 | Label/property intern from graph shard? | **No in PR1** — admin-only |
| 5 | Vertex vs edge label namespaces? | **Separate** — matches `graph-kernel` (`VertexLabelId` / `EdgeLabelId`) |
