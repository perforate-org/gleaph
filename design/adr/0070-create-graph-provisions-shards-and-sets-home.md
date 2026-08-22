# 0070. `CREATE GRAPH` provisions graph shards and sets the home graph

Date: 2026-08-21
Status: accepted (implemented; see Implementation status)
Last revised: 2026-08-22 19:59:13 UTC +0000
Anchor timestamp: 2026-08-22 19:59:13 UTC +0000

## Context

Gleaph today exposes two distinct ways to create a logical graph:

- `register_graph` (a Candid/Router control API) — the dev-mode and provisioned-mode admission
  flow that installs graph canisters and registers them into the Router catalog. When the Router has
  a `provision_canister` configured, it folds the Provision protocol and registers the returned
  shards; when it does not, it registers caller-installed shards directly. This is implemented in
  `crates/router/src/api/control.rs` and `crates/router/src/provisioning/graph.rs`.
- GQL `CREATE GRAPH` — pure schema DDL on `ROUTER_GRAPH_CATALOG` that writes a schema binding at an
  existing federation `GraphId`. It requires the property graph name to already exist in the
  federation catalog (`lookup_graph_id` returns `Some`) or it fails with `GraphNotRegistered`
  (ADR 0013 §2.1). It never provisions a canister.

The CLI `gleaph deploy` and `scripts/deploy-demo-local.sh` drive the dev-mode manual path: install
the Router, graph-index, and graph-shard canisters directly via the management canister, then call
`register_graph` with explicit shard/index principals, and finally call `Account.register_router`.
This exposes canister placement, shard principals, and the graph-shard WASM artifact through the
public API and CLI (`--graph-shard-wasm`).

### Survey of other graph systems

Neo4j separates the physical layer from GQL. A `system` database holds metadata; `CREATE DATABASE`
is a Cypher administration command that creates a physical database partition, distinct from GQL
graph DDL. A default installation has one standard database named `neo4j` that is used without any
`CREATE`; it is the DBMS's default database for the server. A per-user home database is set via
`dbms.setDefaultDatabase`. Enterprise can delete the default only after setting another.

The GQL standard (ISO/IEC 39075:2024) defines `CREATE GRAPH` and `CREATE GRAPH FROM GRAPH TYPE` as
DDL that creates a graph from a graph type. Physical placement, canister issuance, sharding, and
storage are outside the standard's scope and are implementation concerns.

Gleaph already has the "default graph" concept as the caller's home graph. `resolve_graph_context`
falls back from `SESSION SET GRAPH` to the caller's home graph (`crates/router/src/graph_context.rs`),
selected by `GraphRegistryEntry.is_home`. Today only `register_graph` can set `is_home`, so the
public GQL path cannot create the graph that queries resolve to without a Candid call.

## Problem

`CREATE GRAPH` cannot be the sole way a developer creates a usable logical graph. It depends on an
out-of-band `register_graph`/canister placement that:

- leaks shard principals, index placement, and the graph-shard WASM artifact into the public API and
  CLI;
- splits logical-graph creation across GQL DDL (schema) and a control-plane admission (physical);
- leaves home-graph selection on a control-plane flag rather than the DDL that creates the graph.

## Existing Architecture Assessment

The pieces this needs are largely implemented, but not wired to the GQL entry point.

- `provision_graph_flow` already builds the resolved `ProvisionRequest`, sends it to the configured
  Provision canister, and on a fresh `Accepted` with created resources calls `register_provisioned_graph`
  (`crates/router/src/provisioning/graph.rs`). It already registers the graph and its shards, and
  already supports an indexless shard via `Principal::anonymous()` (ADR 0054).
- `register_graph` already folds both modes through one public surface (`crates/router/src/api/control.rs`).
- `resolve_graph_context` already selects a home graph when present.

The missing link is that GQL `CREATE GRAPH` (through `apply_catalog_statement_block`) requires the
graph name to already be federation-registered. Reversing that single prerequisite — so a `CREATE
GRAPH` for an unregistered name triggers the provisioning flow and then writes the schema binding —
would make `CREATE GRAPH` the sole, self-sufficient entry point without new concepts.

## Decision

Adopt a home graph as described below and make GQL `CREATE GRAPH` the single public surface that
creates a logical graph, provisions its graph shard(s), and (when no graph is yet marked `is_home`)
sets the created graph as the home graph.

### 1. `CREATE GRAPH` provisions when the name is not yet registered

`apply_catalog_statement_block` (the generic catalog DDL path) is extended so that, for a
`Statement::CreateGraph`, when `lookup_graph_id(name)` returns `None`:

1. the Router runs the existing admission flow (`provision_graph_flow`) to issue a graph shard
   (and, when the graph type / later declaration requests them, related resources) for that name;
2. on a fresh `Accepted` with created resources, `register_provisioned_graph` registers the graph
   and shards into the Router catalog (unchanged);
3. the schema binding is written at the newly allocated `GraphId`, exactly as today for a
   pre-registered name.

A pre-registered name continues to take the existing binding-only path (no re-provisioning), so a
`CREATE GRAPH` that follows an existing `register_graph` is unchanged. This keeps
`register_graph` as the internal admission routine but removes it from the public graph-creation
story: `CREATE GRAPH` is now the entry that a developer reaches for.

### 2. First created graph becomes the home graph

When `CREATE GRAPH` provisions a brand-new graph and no graph is yet marked `is_home`, the Router
sets `is_home: true` on the newly created graph registry entry. This mirrors Neo4j's default
`neo4j` database: an implicit, usable default for queries that do not name a graph, without an
out-of-band `register_graph` step.

- The home selection is global: there is exactly one `is_home` graph across the Router, enforced by
  the existing `ensure_graph_registration_slot_available` invariant. The first graph created (by any
  caller) becomes the single home; a later `CREATE GRAPH` does not reassign home.
- `SESSION SET GRAPH` still overrides home, exactly as today.

This reuses the existing `resolve_home_graph` / `is_home` mechanism; no new catalog is added.

### 3. Migration allowlist gains vector provisioning (separate ADR)

`CREATE VECTOR INDEX` (ADR 0065) currently stores a targetless `Registered` definition and cannot
provision a canister. Making it provision within a migration is a separate, larger decision (a new
ADR extending ADR 0065) and is explicitly out of scope here. This ADR only makes `CREATE GRAPH` the
self-sufficient graph-creation entry; vector index provisioning remains a later design.

### Authorization

GQL catalog DDL already requires Write or higher via `authorize_adhoc_gql` (`has_catalog_modification`,
ADR 0013 §3). `CREATE GRAPH` that triggers provisioning is the same authorization surface; the Router
is the sole orchestrator of both the schema write and the issuance, so no new RBAC is introduced.

### Crate boundaries

- `gleaph-gql` / `gleaph-gql-planner` remain provider-neutral; no provisioning or IC concept enters
  them.
- The Router owns the DDL → admission bridge. `provision_graph_flow` and `register_provisioned_graph`
  stay in the Router provisioning module.
- The CLI `gleaph deploy` stops requiring `--graph-shard-wasm` and stops installing graph shards
  directly; it installs the Router and registers the account, then delegates graph creation to GQL
  `CREATE GRAPH`.

## Consequences

### Positive

- `CREATE GRAPH` becomes the single, self-contained way to create a usable logical graph: provision,
  register, and (if it is the first graph) make it the home graph in one DDL statement.
- Shard principals, index placement, and graph-shard WASM leave the public API/CLI contract.
- A caller can run a query against a freshly `CREATE GRAPH`ed graph without any other call, matching
  the default-graph ergonomics of Neo4j.
- Reuses the tested `provision_graph_flow` / `register_provisioned_graph` code; no new state machine.

## Trade-offs

- `CREATE GRAPH` now has a side effect (canister issuance) when the name is unregistered. The GQL
  DDL is no longer purely local, so failure atomicity is weaker than a schema-only statement:
  provisioning runs before the binding write, which follows converged `Accepted` or `Replay`
  registration.
- A caller who creates a second graph does not get a second home; that is correct (one home per
  Router).
- The CLI must route graph creation through GQL, so `gleaph deploy` no longer installs graph
  shards itself; this changes the deploy contract for existing demo scripts.

## Migration

1. Implement the `CREATE GRAPH` → admission bridge in `apply_catalog_statement_block`.
2. Set `is_home: true` on fresh provisioning when no graph is yet marked `is_home`.
3. Update `crates/cli/src/deploy.rs` and `scripts/deploy-demo-local.sh` to install Router + account
   and delegate graph creation to `CREATE GRAPH`.
4. Update `design/demo/knowledge-graph-demo.md` and `demo/knowledge` migration seed to use
   `CREATE GRAPH` (it already does) and validate on the local network.
5. Update ADR 0013 (decision §2.1, "This ADR does not auto-register federation canisters from
   catalog DDL alone"), ADR 0054 (status → accepted; close the "CREATE GRAPH does not provision"
   gap), and ADR 0056 (Slice B).

## Design Documentation Impact

- `design/adr/0013-gql-graph-type-catalog-on-router.md` — mark the §2.1 "no auto-registration"
  decision superseded by this ADR.
- `design/adr/0054-provisioned-logical-graph-topology-and-resource-activation.md` — update status to
  accepted; indexless shard and auto-provisioning implemented.
- `design/adr/0056-router-api-surface-layering-and-consolidation.md` — note `register_graph` becomes
  internal; `CREATE GRAPH` is the public entry.
- `design/demo/knowledge-graph-demo.md` — document the knowledge graph is created via `CREATE GRAPH`
  (already true in the migration seed).

## Required Axes Impact

- Encapsulation: provisioning stays in the Router provisioning module; GQL/planner stay neutral.
- Separation of concerns: GQL DDL (create graph) and physical issuance (provisioner) are bridged at
  the Router layer, not merged.
- Invariants: `is_home` single-home-per-Router is preserved; a home graph is only auto-set on first
  graph creation.
- Consistency: the binding and the graph registry entry are written in the same admission flow as
  today, before the binding row for the new `GraphId`.
- Fitness for purpose: removes a split API while reusing the existing provisioner, giving a usable
  default graph for queries.

## Implementation status (2026-08-22)

Implemented and covered by `crates/pocket-ic-tests/tests/adr0070_create_graph_provisioning.rs`
(ad-hoc path + migration path against a real Provision release with the production graph and
index artifacts):

- Bridge: `provisioning::graph::create_graph_admission` — pre-registered names short-circuit to
  the binding-only catalog path; unregistered names run one indexless `GraphShard(0)` bootstrap
  (deployment_id = caller principal, ADR 0068); dev mode (no provisioner) fails closed. Wired into
  both DDL entries: ad-hoc GQL (`run_gql_unchecked` handles pure-catalog-DDL programs before graph
  context resolution, so the first-ever `CREATE GRAPH` needs no home graph) and schema migrations
  (`preprovision_unregistered_create_graphs` before the synchronous co-write; replays re-enter the
  bridge but short-circuit).
- Home: `reconcile_provisioned_graph` marks a fresh bootstrap `is_home` when no home exists yet;
  `ensure_graph_registration_slot_available` remains the single enforcement point (a concurrent
  home fails the registration closed after its entropy await).
- Contract fixes this exposed:
  - Graph shard metadata now accepts `Principal::anonymous()` as the ADR 0054 indexless sentinel
    for `FederationRouting.index_canister` (the wasm init requires router/shard/index set together,
    so absence of an index is expressed as `anonymous`, matching the shard registry). See the
    implementation note in ADR 0054.
  - `RouterIndexLookup::from_shards` treats an indexless shard as "no target" instead of failing
    construction, so scans work before the first index attach.
- Completion convergence: a retry resolves the pending Maps 46/47 indexes to the exact Map 45
  request before the existing-graph early return. Router reconciles an absent graph or missing
  exact shard row, then sends a versionless completion to Provision and releases only locks owned
  by that request. The E2E creates both graphs with the same caller, resolves each exact
  `GraphShard(0)`, asserts distinct graph ids and child canister principals, and proves the first
  graph remains home. The implemented scope remains exactly one `GraphShard(0)`; Property- and
  Vector-index completion are deferred.

Migration step 5 targets: ADR 0013 §2.1 supersession note added; ADR 0054 status updated; ADR 0056
§6 public-surface note added.
