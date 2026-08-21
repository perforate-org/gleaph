# 0071. `CREATE VECTOR INDEX` provisions its vector canister through the migration path

Date: 2026-08-21
Status: proposed
Last revised: 2026-08-21
Anchor timestamp: 2026-08-21 08:28:43 UTC +0000

## Context

ADR 0065 makes `CREATE VECTOR INDEX` Router-owned semantic DDL that stores a targetless
`Registered` definition. Vector canister provisioning exists (ADR 0031 Slice 4) but is reachable
only through the generic GQL path: `crates/router/src/gql.rs` intercepts `CREATE VECTOR INDEX` and
routes it through `create_vector_index` (`crates/router/src/index_catalog.rs:84`), which provisions
a target in provisioned mode. The migration path
(`admin_apply_schema_migration_control`, `crates/router/src/facade/store/schema_migration.rs`)
only consults `gleaph_index_ddl::try_parse` (property indexes, ADR 0059) and never
`try_parse_vector`, so a `CREATE VECTOR INDEX` in a migration is rejected as unsupported.

The `knowledge` demo wants a vector index declared declaratively in a migration (ADR 0070,
`design/demo/knowledge-graph-demo.md`) without out-of-band admin calls. The generic GQL path already
does this; the migration path is the only gap.

## Problem

`CREATE VECTOR INDEX` cannot be created from a migration. A migration that attempts it falls through
to `parse_and_validate_statement`, whose allowlist accepts only `CREATE GRAPH TYPE` / `CREATE GRAPH`,
so the statement is rejected. This forces the demo (and any declarative schema) to reach for the
legacy `admin_register_vector_index` → `set_vector_dispatch_enabled` → `attach_vector_shard` admin
sequence (`scripts/deploy-demo-local.sh:setup_vector_index`) instead of a migration.

## Existing Architecture Assessment

The pieces needed are already implemented:

- `execute_vector_index_ddl_for_graph` (`crates/router/src/index_catalog.rs:52`) dispatches
  `VectorIndexDdlStatement` to `create_vector_index`.
- `create_vector_index` (`:84`) validates the target, provisions a vector canister in provisioned
  mode (via `provision_vector_canister` → `provision_graph_flow`), and registers a targetless
  `Registered` definition in dev mode. It is `pub(crate)` and already used by the generic GQL path.
- The migration apply surface (`admin_apply_schema_migration_control`) already routes one family of
  index DDL (property `CREATE INDEX`, ADR 0059) to a dedicated async driver. Vector DDL is the same
  shape: a single-statement migration that should be routed to the vector execution path rather than
  the schema allowlist.

Unlike the property-index build, a vector index is a **one-shot Router catalog write** — the vector
canister owns embedding bytes and there is no cross-canister backfill or export build (ADR 0064).
So the migration ledger should record it as synchronous `Applied`, not `PendingIndex`.

Provisioning is a remote side effect, but it already has durable idempotency: `provision_graph_flow`
(`crates/router/src/provisioning/graph.rs`) stores the Provision request and replays
`InsertionOutcome::Inserted/Existing`, so a replayed/acknowledged admission resolves the existing
target from the catalog. `provision_vector_canister` already handles the `Accepted` vs
`Replay/Completed` outcomes. No new pending/ack state is needed in the migration ledger.

## Decision

Route a single-statement `CREATE VECTOR INDEX` migration through `execute_vector_index_ddl_for_graph`
and record the migration ledger synchronously as `Applied`.

### 1. Routing

In `admin_apply_schema_migration_control`, consult `gleaph_index_ddl::try_parse_vector` before
`try_parse`. If it returns `Some`, delegate to a new async `apply_vector_index_migration`
(`crates/router/src/facade/store/schema_migration/vector.rs`) that:

1. parses the statement with `try_parse_vector`;
2. validates the checksum over the exact request envelope;
3. resolves the graph from the selector (`Default` → home graph, `Named` → named graph), because a
   vector index is graph-specific;
4. preflights the chain (parent/id sequence) and rejects when another migration is pending;
5. awaits `execute_vector_index_ddl_for_graph(graph_id, statement)` — provisioning (if configured)
   then registering the definition;
6. inserts a ledger record with `resolved_graph: Some(...)`, `profile: [CreateVectorIndex]`, and
   `state: Applied`.

`try_parse` (property) checks the first two idents are `CREATE`/`INDEX`; `CREATE VECTOR INDEX` does
not match that and returns `None`, so `try_parse_vector` is consulted first and routing is
unambiguous.

### 2. Checksum + ledger profile

A new `SchemaMigrationStatementProfile::CreateVectorIndex` variant is added. The vector migration
is single-statement and writes the ledger `Applied` immediately after the catalog write, exactly like
a non-index migration (ADR 0058 co-write). There is no `PendingIndex`/resumable step: if the message
traps, IC rolls back the catalog write and the ledger insert together, and the caller replays the
same envelope (the Provision request store covers idempotency).

### 3. Graph-specific lifecycle

Vector index creation is inherently graph-specific (it resolves a vertex label and an embedding
field in a graph). Therefore the migration must use a graph selector (`Named` or the caller's home
`Default`) and the record carries `resolved_graph: Some`. This matches the property-index contract.

### 4. Dev mode (no provision_canister)

When `provisioning::config::get().is_none()` (dev), `create_vector_index` registers a targetless
`Registered` definition. The migration therefore works in dev mode without auto-provisioning. The
demo's `setup_vector_index` admin sequence remains the operational fallback for attaching a target
in dev; a `CREATE VECTOR INDEX` migration in dev records the definition but does not yet attach a
canister (ADR 0065 fail-closed search semantics).

## Alternatives

### A. Make `try_parse` recognize vector DDL

Fold vector statements into the existing property-index `try_parse`/`IndexDdlStatement`. This
forces `apply_index_migration` to branch on the vector variant and entangles the property-index
resumable lifecycle with the vector one-shot write. It violates the current `index-ddl` separation
(`VectorIndexDdlStatement` is deliberately distinct). Rejected.

### B. Add vector DDL to the generic GQL migration allowlist

Treat `CREATE VECTOR INDEX` as an ordinary GQL catalog statement in `parse_and_validate_statement`
and `statement_profile`. This would require teaching the general GQL parser about the vendor vector
syntax and would route through `apply_catalog_statement_block`, which has no vector provisioning
hook. It mixes concerns across parsing/planning boundaries. Rejected.

## Consequences

### Positive

- `CREATE VECTOR INDEX` in a migration provisions its vector canister (provisioned mode) and
  registers the definition (both modes), matching the generic GQL path and removing the
  out-of-band admin sequence for declarative provisioning.
- Reuses `execute_vector_index_ddl_for_graph` / `create_vector_index`; no new provisioning or
  definition logic.
- The migration ledger stays simple: vector is synchronous `Applied`, distinct from the resumable
  property-index `PendingIndex`.

### Accepted costs

- Adding a Candid enum variant (`CreateVectorIndex`) is a breaking wire change; per pre-production
  policy a fresh state/reinstall is acceptable.
- A dev-mode migration registers a targetless definition and does not attach a canister; activating
  search in dev still needs the admin attach step (ADR 0065 non-goal).

## Migration

1. Add `SchemaMigrationStatementProfile::CreateVectorIndex`.
2. Add `apply_vector_index_migration` in `store/schema_migration/vector.rs` and route
   `try_parse_vector` before `try_parse` in `admin_apply_schema_migration_control`.
3. Update the CLI (`validate_gql`, `graph_selector_for_manifest`, `resolved_graph_matches_profile`)
   to recognize and require a resolved graph for `CreateVectorIndex`.
4. Add a `CREATE VECTOR INDEX` migration to the `knowledge` demo.
5. Update ADR 0065 status and `design/adr/README.md`.

## Design Documentation Impact

- `design/adr/README.md` — index this proposed ADR.
- `design/adr/0065-create-vector-index-ddl.md` — update status: migration provisioning and
  activated search via `CREATE VECTOR INDEX` now land through the migration path.
- `design/demo/knowledge-graph-demo.md` — the vector index is declared in a migration.

## Required Axes Impact

- Encapsulation: Router owns names, authorization, provisioning, and catalog writes; the vector
  canister owns embedding bytes; GQL/planner crates stay neutral.
- Separation of concerns: vector DDL stays in `gleaph-index-ddl` (`VectorIndexDdlStatement`); the
  migration routing stays in the Router schema-migration module.
- Invariants: one vector index per embedding field per graph, exact-definition `IF NOT EXISTS`,
  fail-closed readiness, and synchronous `Applied` are preserved.
- Consistency: one durable vector definition and one migration ledger record; Provision
  idempotency covers the remote side effect.
- Fitness for purpose: makes declarative vector provisioning self-sufficient for the demo.
