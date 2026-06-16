# PR1 — `gleaph-router` Candid API

Control-plane canister for federated Gleaph graphs. PR1 establishes the router as metadata authority
(graph registry, shard registry, logical vertex placement, label/property intern) without GQL execution
or vertex migration.

**Related decisions (PR1):**

- `LogicalVertexId = nat64` — stable global vertex identity (router-allocated).
- `ShardId = nat32` — partition id; not a substitute for logical identity.
- Index posting keys stay **physical**: `(property_id, encoded_value, shard_id, local_vertex_id)`.
- Shard registry moves from `gleaph-graph-index` to the router; index keeps shard/canister attachment map only.
- Breaking changes are acceptable (no migration from prior federation v1 wire formats).

---

## Shared types

Portable types live in `gleaph-graph-kernel` (`federation`, `index`). Router- and registry-specific
records are defined here for Candid export.

```candid
type LogicalVertexId = nat64;
type ShardId = nat32;
type LocalVertexId = nat32;

type PhysicalVertexLocation = record {
  shard_id : ShardId;
  local_vertex_id : LocalVertexId;
};

type VertexPlacement = variant {
  Active : PhysicalVertexLocation;
  // Reserved for PR4; not implemented in PR1.
  Migrating : record { epoch : nat64 };
};

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
  logical_graph_name : text;
  registered_at_ns : nat64;
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
  VertexNotFound;
  PlacementAlreadyCommitted;
  UnallocatedLogicalVertex;
  Internal : text;
};
```

---

## Init

| Method | Args | Returns | Notes |
|--------|------|---------|-------|
| `init` | `RouterInitArgs` | — | Canister install |

```candid
type RouterInitArgs = record {
  controllers : vec principal;
};
```

---

## Query (read)

| Method | Args | Returns | Auth (PR1) | Notes |
|--------|------|---------|------------|-------|
| `whoami` | — | `principal` | public | Same pattern as `gleaph-graph` |
| `resolve_graph` | `graph_name : text` | `Result<GraphRegistryEntry, RouterError>` | caller | Owner or admin only |
| `resolve_shard` | `shard_id : ShardId` | `Result<ShardRegistryEntry, RouterError>` | **public** | Replaces index `resolve_shard_principal` |
| `resolve_placement` | `logical_vertex_id : LogicalVertexId` | `Result<VertexPlacement, RouterError>` | **public** | PR1: `Active` only |
| `lookup_vertex_label_id` | `name : text` | `Result<VertexLabelId, RouterError>` | public | |
| `lookup_edge_label_id` | `name : text` | `Result<EdgeLabelId, RouterError>` | public | |
| `lookup_property_id` | `name : text` | `Result<PropertyId, RouterError>` | public | |
| `reverse_vertex_label_name` | `label_id : VertexLabelId` | `Result<text, RouterError>` | public | Optional; planner/debug |
| `reverse_edge_label_name` | `label_id : EdgeLabelId` | `Result<text, RouterError>` | public | Optional; planner/debug |
| `reverse_property_name` | `property_id : PropertyId` | `Result<text, RouterError>` | public | Optional; planner/debug |

**Public read APIs:** `resolve_shard`, `resolve_placement`, and metadata lookups expose registry and
placement directory to any caller that can reach the canister (same policy as `gleaph-graph-index`
lookups today). Gate at a higher layer if needed.

`resolve_graph_canister` is intentionally omitted; use `resolve_graph` and read `canister_id` from the
entry when graph registry entries include a graph shard principal (future multi-shard graphs may extend
this).

---

## Update — router admin

Caller must be in `controllers` (set at `init`).

| Method | Args | Returns | Notes |
|--------|------|---------|-------|
| `admin_register_graph` | `entry : GraphRegistryEntry` | `Result<(), RouterError>` | |
| `admin_update_graph_status` | `graph_name : text`, `status : GraphStatus`, `version : nat64` | `Result<(), RouterError>` | Optimistic `version` |
| `admin_register_shard` | `AdminRegisterShardArgs` | `Result<(), RouterError>` | Router registry + IC call to index |
| `admin_unregister_shard` | `shard_id : ShardId` | `Result<(), RouterError>` | PR1: registry removal + index shard/canister detach |
| `admin_intern_vertex_label` | `name : text` | `Result<VertexLabelId, RouterError>` | Idempotent; `0` reserved |
| `admin_intern_edge_label` | `name : text` | `Result<EdgeLabelId, RouterError>` | Idempotent; max `0x7FFF` catalog id |
| `admin_intern_property` | `name : text` | `Result<PropertyId, RouterError>` | Idempotent intern |

```candid
type AdminRegisterShardArgs = record {
  shard_id : ShardId;
  graph_canister : principal;
  index_canister : principal;
  logical_graph_name : text;
};
```

**`admin_register_shard` side effect:** after router stable insert, the router calls the index canister:

```text
index.admin_attach_shard_canister(shard_id, graph_canister)
```

---

## Update — graph shard

Caller must be the `graph_canister` principal registered for its shard.

| Method | Args | Returns | Notes |
|--------|------|---------|-------|
| `allocate_logical_vertex_id` | — | `Result<LogicalVertexId, RouterError>` | Monotonic counter |
| `commit_vertex_placement` | `CommitVertexPlacementArgs` | `Result<(), RouterError>` | After local vertex insert |

```candid
type CommitVertexPlacementArgs = record {
  logical_vertex_id : LogicalVertexId;
  local_vertex_id : LocalVertexId;
};
```

**Rules:**

- `shard_id` is inferred from the caller principal (not passed in args).
- `logical_vertex_id` must be the shard's current pending allocation from `allocate_logical_vertex_id`.
- Router records `logical_vertex_id -> Active { shard_id, local_vertex_id }`.

**Deferred to PR2+:** `release_logical_vertex_id` / tombstone on vertex delete.

---

## PR1 export summary

```
init(RouterInitArgs)

// query
whoami() -> principal
resolve_graph(text) -> Result<GraphRegistryEntry, RouterError>
resolve_shard(ShardId) -> Result<ShardRegistryEntry, RouterError>
resolve_placement(LogicalVertexId) -> Result<VertexPlacement, RouterError>
lookup_vertex_label_id(text) -> Result<VertexLabelId, RouterError>
lookup_edge_label_id(text) -> Result<EdgeLabelId, RouterError>
lookup_property_id(text) -> Result<PropertyId, RouterError>
reverse_vertex_label_name(VertexLabelId) -> Result<text, RouterError>   // optional
reverse_edge_label_name(EdgeLabelId) -> Result<text, RouterError>       // optional
reverse_property_name(PropertyId) -> Result<text, RouterError>          // optional

// update — admin
admin_register_graph(GraphRegistryEntry) -> Result<(), RouterError>
admin_update_graph_status(text, GraphStatus, nat64) -> Result<(), RouterError>
admin_register_shard(AdminRegisterShardArgs) -> Result<(), RouterError>
admin_unregister_shard(ShardId) -> Result<(), RouterError>
admin_intern_vertex_label(text) -> Result<VertexLabelId, RouterError>
admin_intern_edge_label(text) -> Result<EdgeLabelId, RouterError>
admin_intern_property(text) -> Result<PropertyId, RouterError>

// update — graph shard
allocate_logical_vertex_id() -> Result<LogicalVertexId, RouterError>
commit_vertex_placement(CommitVertexPlacementArgs) -> Result<(), RouterError>
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
| Add | `admin_attach_shard_canister(ShardId, principal) -> Result<(), text>` — **router caller only** |
| Change | `posting_*` / `PostingHit.shard_id` → `ShardId` (`nat32`) |
| Change | `PostingKey` encoding — `shard_id` 4 bytes; consider `POSTING_KEY_MAGIC = 2` |

```candid
type IndexInitArgs = record {
  controllers : vec principal;
  router_canister : principal;
};
```

---

## Type placement (Rust)

| Types | Crate |
|-------|-------|
| `ShardId`, `LogicalVertexId`, `PhysicalVertexLocation`, `VertexPlacement` | `gleaph-graph-kernel::federation` |
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
