# 0056. Router API surface layering and consolidation

Date: 2026-08-01
Status: implemented
Last revised: 2026-08-15 22:59:22 UTC +0000
Anchor timestamp: 2026-08-15 22:59:22 UTC +0000

## Context

The Router exposes about 95 production Candid methods (103 exported including `init` /
`post_upgrade` and six test seams). A caller audit on 2026-08-01 across tests (`pocket-ic-tests`),
the social-demo seed script, the gateway canister, the codegen CLI, and canister-internal callers
found roughly 40 methods with zero callers, and several groups that duplicate one another:

- **Internal design leaks into the public surface.** Shards (`register_shard`, `resolve_shard`,
  per-shard backfill args, per-shard result vectors), backfill machinery (`label_backfill_step`,
  `vertex_property_backfill_step`, `edge_backfill_step`, `label_stats_projection_step` and their
  status queries), vector physical design (`index_id`, dispatch target, activation, partition
  health, slab stats, centroid cache, rebuild steps), canister principals, id-encoding keys, and the
  provisioning protocol are all visible to clients.
- **Redundant entrypoint families.** `gql_query` / `gql_query_with_consistency` and
  `prepared_query` / `prepared_query_with_consistency` differ only by a `ReadMode` argument;
  `gql_execute` (non-idempotent) and `force_gql_execute` have no callers; the non-idempotent
  `prepared_update` / `prepared_query_as_update` have no callers; the four `prepared_upsert*`
  variants collapse to one.
- **The provisioning flow does not complete.** `provision_graph` sends an envelope and the
  `router_ack` callback advances only the `RouterProvisioningRequest` record. The graph registry is
  never written: `ProvisioningState::Pending` / `Failed` are never set anywhere in `crates/router`.
  A provisioned graph is never actually registered.
- The JS SDK and CDK now connect to the Router operation surface; generated bindings and conformance
  tests are maintained as part of the same breaking release set.
- The project is pre-release; breaking public-surface changes are accepted without compatibility
  wrappers (precedent: ADR 0049's 2026-07-31 correction).

## Decision

### Slice A follow-up: explicit operation names (ADR 0057, 2026-08-02)

The accepted Slice A layering remains the ownership decision, while ADR 0057 fixes the operation
names and typed atomic-insert receipt at the L1 boundary. The current operation-execution surface
is `gql_query`, `gql_mutate`, `prepared_query`, `prepared_mutate`, `atomic_insert`, `bulk_load`,
`mutation_status`, `atomic_insert_status`, and `bulk_load_status`. The retired cursor-list endpoint
has no compatibility alias; durable initial loading is owned by the `bulk_load` lifecycle.

Restructure the Router public surface into three layers owned by separate modules under `api/`, with
the API boundary treating the _conceptual graph_ as the unit of operation and hiding federation
internals.

### 1. Surface layering and module layout

| Layer                  | Module              | Audience                                            | Contents                                                                                    |
| ---------------------- | ------------------- | --------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| L1 client (data plane) | `api/client.rs`     | applications, new SDK                               | GQL read/write, prepared operations, `vector_search`, mutation status                       |
| L2 control plane       | `api/control.rs`    | CLI, graph-admin UI                                 | graph lifecycle, RBAC, schema, vector semantic management, maintenance, diagnostics summary |
| L3 federation          | `api/federation.rs` | graph/index/vector canisters, operator deep tooling | shard resolution, catalog, `router_ack`, vector wiring, physical diagnostics                |

Layout:

```
crates/router/src/
  lib.rs                init / post_upgrade / ic_cdk::export_candid!() only
  api.rs                module declarations + pub use (surface index)
  api/client.rs         L1
  api/control.rs        L2
  api/federation.rs     L3
```

- Candid annotations (`#[query]` / `#[update]`) move into the layer modules. `ic_cdk::export_candid!()`
  collects `#[candid_method]` functions crate-wide, so `lib.rs` keeps only `init`, `post_upgrade`,
  and the export macro. `init` / `post_upgrade` remain in `lib.rs` because they are canister
  bootstrap, not API surface. This is the repository's first use of annotations outside `lib.rs`
  (the graph crate keeps them in `lib.rs`); verify with a smoke build at the start of Slice A. If
  collection fails, fall back to thin annotated wrappers in `lib.rs` delegating to the `api` modules.
- L3 is documented as client-invisible. Candid cannot hide methods from the `.did`, so "invisible"
  means: excluded from client-facing documentation, restricted by existing RBAC / caller guards
  (e.g., graph and index canisters are registered principals), and absent from the new SDK.
- `api.rs` uses the module-file style (`api.rs` + `api/` directory), matching the existing router
  convention (`facade.rs` + `facade/`, `federation.rs` + `federation/`). `canister.rs` is retired.
- The three `api` modules are siblings: they do not call each other; cross-domain orchestration goes
  through `facade` / `gql` / `prepared` / `provisioning`. `api.rs` is a pure index (`pub use` only)
  with no shared state.
- The seven `facade/store/tests.rs` call sites that exercise `crate::canister::vector_search`
  orchestration move into the `api` layer test modules during Slice A, keeping domain test files
  domain-only.

### 2. Naming conventions

1. Reads: `get_*` (single), `list_*` (collection), `scan_*` (cursor pages). Retires `resolve_*`,
   `lookup_*`, `reverse_*`, and bare `*_catalog` names.
2. Writes use the verb directly: `register_`, `unregister_`, `ensure_`, `grant_`, `index_`, `set_`,
   `attach_`, `start_`, `publish_`, `abort_`, `reset_`, `drop_`, `warm_`, `clear_`.
3. Budget-driven drivers are `advance_*` (one call advances one bounded unit), retiring `*_step`.
4. The `admin_` prefix is dropped; required roles are documented per method and enforced in code.
5. The `gql_` prefix is retained for the user-facing data plane, distinguishing it from the control
   plane.
6. Idempotency is not encoded in names; `client_mutation_key` is the mechanism and is documented as
   a contract.
7. Consistency is an argument: `_with_consistency` variants are merged into a `ReadMode` parameter.
   `ReadMode::Canonical` (unimplemented, always rejected) is removed; `Eventual` and
   `AtLeast(token)` remain.

### 3. L1 client surface (data plane, ~11)

| New API                                                        | Kind            | Replaces                                                                              |
| -------------------------------------------------------------- | --------------- | ------------------------------------------------------------------------------------- |
| `gql_query(query, params, read_mode)`                          | composite query | `gql_query` + `gql_query_with_consistency`                                            |
| `gql_mutate(query, params, client_mutation_key)`               | update          | prior `gql_execute_idempotent`/non-idempotent split; no compatibility alias           |
| `atomic_insert(request)`                                       | update          | retired typed public insert names; no compatibility alias                             |
| `bulk_load(command)`                                           | update          | durable start/append/finalize/abort initial-load lifecycle                            |
| `mutation_status(graph, client_mutation_key)`                  | query           | prior `get_mutation_status`                                                           |
| `atomic_insert_status(graph, client_mutation_key)`             | query           | new family-specific ordered receipt lookup                                            |
| `bulk_load_status(graph, client_load_key, receipt_cursor)`     | query           | durable bulk job state and paged committed receipts                                   |
| `prepare(graph, name, query, metadata: opt PreparedOperation)` | update          | `prepared_upsert_with_metadata`; removes `prepared_upsert` and superseded variants    |
| `drop_prepared(name)`                                          | update          | `prepared_delete`                                                                     |
| `list_prepared(graph_name)`                                    | query           | `prepared_manifest` (return type `PreparedManifest` unchanged)                        |
| `prepared_query(name, params, sort, read_mode)`                | composite query | prior `execute_prepared` + consistency variant                                        |
| `prepared_mutate(name, params, client_mutation_key)`           | update          | prior `execute_prepared_update`/`prepared_update_idempotent`; no compatibility alias  |
| `vector_search(graph, index_name, query, top_k)`               | composite query | `vector_search` (addresses the vector index by `(graph, index_name)`, not `index_id`) |

Graph resolution follows the current model: the GQL family (`gql_query` / `gql_mutate`) resolves
the graph from the program (`USE GRAPH`, including multi-graph segments and joins) and takes no
positional graph argument; prepared operations are graph-scoped at
registration (`prepared_query` / `prepared_mutate` resolve via the plan's graph binding); explicit graph arguments
appear where the current surface already has them (`atomic_insert`, `mutation_status`,
`bulk_load`, `bulk_load_status`, `vector_search`).

### 4. L2 control surface (~23; CLI and graph-admin UI)

| Domain          | API                                                                                                                                                       |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Graph lifecycle | `register_graph(intent)`, `unregister_graph(graph)`, `list_graphs()`, `get_graph(name)`, `get_graph_health(name)`                                         |
| RBAC            | `whoami()`, `my_role()`, `grant_role(args)`                                                                                                               |
| Schema          | `ensure_vertex_label`, `ensure_edge_label`, `ensure_properties`, `index_vertex_property`, `index_edge_property`                                           |
| Vector semantic | `create_vector_index(name, spec)`, `drop_vector_index(name)`, `list_vector_indexes(graph)`, `rebuild_vector_index(name)`, `get_vector_index_health(name)` |
| Maintenance     | `get_graph_sync_status(graph)`, `advance_backfill(graph, kind, max_work)`, `list_backfill_status(graph)`                                                  |
| Ingestion       | `ingest_vertex_embeddings(graph, items)`                                                                                                                  |
| Diagnostics     | `get_stable_memory_stats(graph)`                                                                                                                          |

Notes:

- `list_graphs` is new; the Router has no graph enumeration today.
- `advance_backfill(graph, kind, max_work)` consolidates `admin_label_backfill_step`,
  `admin_vertex_property_backfill_step`, `admin_edge_backfill_step`, and
  `admin_label_stats_projection_step` behind a `BackfillKind` enum; the Router iterates shards
  internally (shard ids are not exposed at L2). `list_backfill_status(graph)` consolidates the three
  `admin_list_*_backfill_status` queries into one kind-keyed view.
- `get_graph_sync_status(graph)` replaces `admin_index_sync_status` (aggregated across shards).
- Schema seams (`ensure_*`, `index_*`) remain the CLI/bootstrap path until GQL schema DDL is the
  primary surface; tests and seed flows depend on them.
- `get_graph` returns the full `GraphRegistryEntry` (owner, admins, canister id) because the admin
  UI needs the admins list; the graph's own canister id is the graph's address, not federation
  topology.

### 5. L3 federation surface (client-invisible, ~40)

| Group                       | API                                                                                                                                                                                                                                                                      |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Registry / catalog          | `get_graph_id`, `get_shard`, `list_shards`, `get_indexed_property_catalog`, `get_id_encoding_key`, `register_shard`, `unregister_shard`, `update_graph_status`, `check_registry_invariants`, `sweep_expired_mutation_keys`, `router_ack`                                 |
| Vector wiring               | `set_vector_index_target`, `attach_vector_shard`, `get_vector_index_target`, `get_vector_index_status`, `set_vector_dispatch_enabled`, `get_vector_dispatch_enabled`, `advance_vector_index_backfill`                                                                    |
| Vector physical diagnostics | `get_vector_partition_health`, `scan_partition_health`, `get_vector_slab_stats`, `scan_slab_stats`, `get_vector_centroid_cache`, `warm_vector_centroid_cache`, `clear_vector_centroid_cache`                                                                             |
| Maintenance deep            | `set_vector_maintenance_policy`, `disable_vector_maintenance_policy`, `delete_vector_maintenance_policy`, `get_vector_maintenance_policy`, `list_vector_maintenance_policies`, `advance_vector_maintenance`, `reset_vector_maintenance`, `get_vector_maintenance_status` |
| Rebuild manual              | `start_vector_rebuild`, `get_vector_rebuild_status`, `advance_vector_rebuild`, `publish_vector_rebuild`, `abort_vector_rebuild`, `advance_vector_rebuild_cleanup`                                                                                                        |
| Backfill                    | `reset_backfill_claim`                                                                                                                                                                                                                                                   |

Notes:

- These are the graph/index/vector canister call sites (shard resolution, catalogs), the
  Provision-to-Router callback (`router_ack`), and operator deep tooling. The vector canister stays
  router-guarded, so the L3 forwards are the only operator entry points to it.
- `admin_start_vector_rebuild_if_recommended` is removed from the Router surface; the push model
  (`advance_vector_maintenance`) applies the recommendation internally. `admin_vector_maintenance_status`
  is removed because `get_vector_maintenance_status` (composite) embeds the forwarded state.

### 6. `register_graph` folds provisioning

`register_graph(intent)` replaces `admin_register_graph` and `provision_graph` on the public
surface; the provisioning protocol becomes an internal implementation detail. `router_ack` remains
an L3 callback. Implemented in two slices:

**Slice A — dev-mode full registration + provisioning-state machine (no wire change).**

- `register_graph(intent)` where the intent shape differs by mode:
  - dev (no `provision_canister` configured): `{ graph_name, owner, admins, is_home : bool, shards:
vec record { shard_id : nat32; graph_canister : principal; index_canister : principal } }` — the
    caller installs the shard canisters and passes their principals (mirroring the existing
    `AdminRegisterShardArgs` shape); the graph entry and its shards are committed synchronously in
    one call. `is_home` is the legacy ADR 0011 home-graph designation (the default graph for
    callers without an explicit `USE GRAPH`); it is a client-visible graph property, so it stays in
    the intent while federation topology (shards, canister ids) stays internal.
    This is the main flow for E2E tests, CLI, and UI.
  - provisioned (`provision_canister` configured): `{ graph_name, owner, admins, requested_resources }`
    — returns `NotImplemented` until Slice B. The current provision flow never completes a graph, so
    pausing it has no operational cost.
- The `ProvisioningState` write path (`Pending { request_id }` / `Failed` / `None`) on
  `GraphRegistryEntry` is implemented and fixed by store-level unit tests (the field is currently
  never written in `crates/router`). Resolution excludes non-`None` provisioning states: a `Pending`
  or `Failed` entry is not resolvable for data-plane dispatch (`gql_query` / `gql_mutate` /
  prepared / `vector_search`) until it reaches `ProvisioningState::None`. No new `GraphStatus`
  variant is required; `GraphStatus` stays `Active` / `ReadOnly` / `Deprecated` / `Deleting`. The
  guard is enforced once in the registry resolution path (`RouterStore::resolve_graph_id*`), which
  every data-plane entrypoint shares, not in individual entrypoints.
- `admin_register_graph` is removed from the public surface. `provision_graph` is demoted to a
  documented-internal L3 seam through Slice A (so the `adr0035` outbound/ack E2E coverage keeps
  running unchanged) and is removed in Slice B when the new flow replaces it.

**Slice B — provisioning integration (implemented 2026-08-15; synchronous registration).**

The original Slice B plan routed the deployed topology through the ack callback
(`RouterProvisionAck.shards`, reserved_graph_id link, non-async register on ack). The implemented
integration instead folds provisioning into `register_graph` **synchronously**, because the
`accept_envelope` deploy is synchronous (ADR 0035 Slice 8): the Provision canister returns the
installed canister ids in `created_resources` on the admission response, so there is no separate
ack to carry topology. The synchronous choice keeps admission → deploy → register in one call
and avoids an intermediate "admitted but not yet registered" state. Deferred to a future slice:
a fully asynchronous saga (heartbeat/outbound-ack driven deploy) would reintroduce the ack
topology transport and split registration onto the Router's `router_ack` callback.

Implemented behavior:

- `register_graph(intent)` is the single public graph-creation surface for both modes. With a
  `provision_canister` configured it folds into the provisioning flow; `deployment_id` derives
  from the caller's owner principal (ADR 0068), `request_fingerprint` from the graph name, and
  `release_id` defaults to `"default"`.
- The shared admission flow lives in `crate::provisioning::graph::provision_graph_flow`: it seeds
  the `RouterProvisioningRequest`, sends `accept_envelope`, and on a fresh `Accepted` with non-empty
  `created_resources` registers the graph and its shards (via
  `admin_register_graph_with_random_key` / `admin_register_shard`).
- `provision_graph` remains as a thin L3 seam that delegates to the same flow, retained so the
  `adr0035_router_outbound_accept_envelope.rs` E2E coverage runs unchanged.
- `unregister_graph` is unchanged for now; symmetric Provision teardown notification is a future
  lifecycle slice (ADR 0037).

### 7. `list_graphs` / `get_graph` / `get_graph_health` return types

View types live in `crates/router/src/types.rs` (they are Router surface views, not domain types;
`GraphStatus`, `ProvisioningState`, `GraphRegistryEntry` stay in `gleaph-gql-ic`).

```candid
type GraphSummary = record {
  graph_id : nat64;
  graph_name : text;
  status : GraphStatus;
  provisioning_state : ProvisioningState;
  shard_count : nat32;          // derived from the registry, no cross-canister calls
  updated_at_ns : nat64;
};

type GraphHealthView = record {
  graph : GraphSummary;
  reachable_shard_count : nat32;        // shards that answered a liveness check
  index_sync_converged : bool;          // all shards' index-sync aggregated
  vector_index_count : nat32;
  unhealthy_vector_indexes : vec text;  // names only; detail lives at L3
  notes : vec text;                     // bounded, best-effort repair guidance
};
```

- `list_graphs() -> vec GraphSummary` is registry-local and cheap (it is polled by UI/CLI).
- `get_graph(name) -> GraphRegistryEntry` is the full row for detail views and the roles page.
- `get_graph_health(name) -> GraphHealthView` is a composite query, best-effort, bounded; it does
  not embed per-shard or physical detail (those stay at L3).

## Consequences

- Client-visible surface shrinks from ~95 to ~34 methods (L1 + L2); L3 (~40) remains documented as
  federation-internal. Total production surface drops to ~74 plus `init` / `post_upgrade`.
- Breaking change, no compatibility wrappers (pre-release; same stance as ADR 0049).
- Update targets on implementation: `pocket-ic-tests/src/lib.rs` helpers,
  `crates/codegen/src/cli.rs` (uses `prepared_manifest` → `list_prepared`), and the demo loader, which moved to the Gleaph CLI
  (`crates/cli/src/load.rs` drives durable `bulk_load` / `bulk_load_status`; the former
  `apply-social-load.mjs` JS driver was retired). The social frontend later dropped committed
  actor bindings for the SDK-direct client (2026-08-06); `scripts/check-router-and-graph-candid.sh`
  now asserts the live Candid surface from freshly built wasm instead of diffing checked-in bindings.
- Design docs to update on implementation: `design/index/derived-state-query-semantics.md`
  (entrypoint table), `design/architecture/acid-roadmap.md`, `design/security/rbac-and-prepared.md`,
  `design/storage/stable-memory-inventory.md`; `crates/router/docs/pr1-candid.md` is superseded and
  marked deprecated.
- The redesigned SDK targets L1 plus the L2 schema subset; it never calls L3.
- Useful-but-unused operator functionality is retained at L3 rather than deleted (vector diagnostics,
  manual rebuild control, maintenance policy CRUD).

## Alternatives considered

- **Keep the `admin_` prefix** as an operator scanning aid. Rejected: roles are documented per
  method and enforced in code; the prefix obscured the function name.
- **Keep `canister/` + `mod.rs`.** Rejected: the module-file style (`api.rs` + `api/`) matches the
  router's existing `facade.rs` / `federation.rs` convention, and `canister` conflates the public
  surface with the canister artifact (the workspace has five canisters).
- **One flat admin surface instead of three layers.** Rejected: clients, CLI/UI, and canisters have
  genuinely different needs; a single surface cannot be both minimal for clients and complete for
  federation.
- **Fully autonomous maintenance with no manual override.** Rejected: operators still need
  graph-level repair triggers and health views; the manual drivers are retained at L2/L3.
- **Delete L3 entirely.** Rejected: graph/index/vector canisters depend on these calls, and the
  vector canister is router-guarded, so the forwards are the only operator entry points.
- **Keep provisioning as a separate public API.** Rejected: it leaks the protocol and, as
  implemented, never completes a graph; folding it into `register_graph` makes the surface express
  intent while the protocol stays internal.
