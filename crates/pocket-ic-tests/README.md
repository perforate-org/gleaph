# PocketIC federation tests

## Prerequisites

- PocketIC server: `.pocket-ic/pocket-ic` (fetched at build time if missing or version mismatch). The tests execute it through `.pocket-ic/pocket-ic-launcher`, which closes unrelated inherited file descriptors before starting PocketIC. `POCKET_IC_BIN` may override it only when the binary reports the same `pocket-ic-server` version as the `pocket-ic` crate dependency; a mismatched override is ignored (stale shell exports otherwise cause composite-query timeouts). Prefer leaving `POCKET_IC_BIN` unset for local runs so the launcher stays in effect.
- `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`

## Run

```bash
# Fast local smoke: one PocketIC server, one two-shard federation, one indexed GQL query
cargo test -p gleaph-pocket-ic-tests --test smoke -- --nocapture

# Full PocketIC gate
cargo test -p gleaph-pocket-ic-tests -- --nocapture
```

PocketIC tests start local PocketIC server / replica processes. The Rust test
harness runs tests in parallel inside each integration-test binary, and the full
suite supports that default execution mode. On slower or resource-constrained
machines, use the `smoke` target or a focused test filter for faster iteration.

The full suite is intentionally broad: most tests create a fresh PocketIC
instance and reinstall the Router, Index, and Graph canisters to preserve test
isolation across upgrade, timer, recovery, and fault-injection contracts. That
is the main runtime cost. Keep the `smoke` target small and do not add
failure-mode or upgrade coverage there unless it is required for the daily
developer loop.

PocketIC 14.0.0 starts the server in a background process with `--hard-ttl 600`.
The server is shared only within a single Rust test binary process and may remain
for up to 10 minutes after the test exits or is interrupted. If local runs become
unexpectedly slow after `^C`, check for orphaned servers from this checkout with
`pgrep -af 'crates/pocket-ic-tests/.pocket-ic/pocket-ic'` and stop those stale
processes before rerunning.

Some integrated terminals can leave editor-owned file descriptors open in child
processes. If those descriptors reach PocketIC's sandbox launcher, canister
installation may appear to hang after the server prints `listening on port ...`.
The generated `.pocket-ic/pocket-ic-launcher` closes non-stdio descriptors before
execing the real PocketIC server to keep terminal/editor state out of sandbox
children.

## Status

| Test                                                                    | Coverage                                                |
| ----------------------------------------------------------------------- | ------------------------------------------------------- |
| `graph_seed_dispatch::graph_execute_plan_query_skips_index_scan_with_seed_bindings` | Federated graph `execute_plan_query` + `seed_bindings_blob` |
| `graph_seed_dispatch::graph_execute_plan_query_rejects_index_scan_without_seeds` | Federated graph rejects bare `IndexScan` without router seeds |
| `router_gql_query::single_shard_identity_lifecycle` | Single-shard identity lifecycle: NodeScan, `GlobalVertexId`, and `ELEMENT_ID` round-trip |
| `router_gql_query::single_shard_vertex_index_lifecycle` | Single-shard vertex-index lifecycle: CREATE INDEX, indexed equality, two-index intersection, DROP INDEX, scan fallback, idempotent IF EXISTS drop, missing DROP NotFound |
| `router_gql_query::single_shard_generic_edge_index_lifecycle` | Single-shard generic edge-index lifecycle: CREATE INDEX, indexed property equality, DROP INDEX, scan fallback |
| `router_gql_query::single_shard_knowledge_map_relationship_rows` | Helper-seeded `KNOWS` relationship row material (source/edge/target ids + weight) |
| `router_gql_query::single_shard_knowledge_map_relationship_rows_from_insert` | GQL `INSERT`-created `KNOWS` relationship row material (source/edge/target ids + weight) |
| `router_gql_query::single_shard_knowledge_map_fan_out` | Full knowledge-map demo graph fan-out (26 seeded demo edges) |
| `router_gql_query::single_shard_pointing_right_edge_index_lifecycle` | Single-shard pointing-right edge-index lifecycle: directed CREATE INDEX DDL, indexed property equality, DROP INDEX, scan fallback |
| `router_gql_query::single_shard_undirected_edge_index_lifecycle` | Single-shard undirected edge-index lifecycle: anonymous symmetric expansion without index, undirected CREATE INDEX DDL, indexed equality, directed insert does not seed undirected index, DROP INDEX, scan fallback |
| `router_gql_query::federated_vertex_index_lifecycle` | Federated vertex-index lifecycle: hit-shard-only routing, cross-shard merge, two-property intersection with partial-match sieve, and `DROP INDEX` removing the federated anchor |
| `router_gql_query::federated_edge_index_lifecycle` | Federated edge-index lifecycle: undirected DDL, pointing-right DDL, generic directed DDL, exact source/destination shard decoding, and `DROP INDEX` removing the federated anchor |
| `router_graph_type_catalog::catalog_create_graph_type_returns_zero_rows` | ADR 0013: `CREATE GRAPH TYPE` on router stable catalog |
| `router_graph_type_catalog::catalog_typed_binding_persists_across_calls` | ADR 0013: `CREATE GRAPH … TYPED` + `gql_query` after catalog DDL |
| `router_graph_type_catalog::catalog_create_graph_unregistered_name_rejected` | ADR 0013: `CREATE GRAPH` without federation registration → `NotFound` |
| `router_graph_type_catalog::catalog_drop_graph_type_cascades_typed_binding` | ADR 0013: `DROP GRAPH TYPE` removes type; rebinding fails |
| `router_graph_type_catalog::catalog_typed_schema_rejects_undirected_match_on_directed_edge` | ADR 0013: typed schema rejects `MATCH` edge direction mismatch at ingress |
| `adr0030_constraint_dispatch::create_constraint_publishes_and_enforces` | ADR 0030 slice 8: public `CREATE CONSTRAINT` succeeds and enforces (duplicate INSERT → `UniquenessViolation`) |
| `adr0030_constraint_dispatch::duplicate_create_constraint_conflicts_unless_if_not_exists` | ADR 0030 slice 8: re-declare → `Conflict`; `IF NOT EXISTS` is an idempotent no-op |
| `adr0030_constraint_dispatch::create_constraint_on_existing_label_is_rejected` | ADR 0030 slice 8: `CREATE CONSTRAINT` on an existing label → `Conflict` (declare-on-empty) |
| `adr0030_constraint_dispatch::malformed_create_constraint_is_invalid_argument` | ADR 0030 slice 8: malformed constraint DDL → `InvalidArgument` |
| `adr0030_constraint_dispatch::edge_create_constraint_is_invalid_argument` | ADR 0030 slice 8: unsupported edge `CREATE CONSTRAINT` over public ingress → `InvalidArgument` |
| `adr0030_constraint_dispatch::create_constraint_on_query_entrypoint_is_path_mismatch` | ADR 0030: constraint DDL on the `query` entrypoint → `ExecutionPathMismatch` |
| `adr0030_constraint_dispatch::create_constraint_requires_authorization` | ADR 0030: non-admin `CREATE CONSTRAINT` → `Forbidden` before any other check |
| `adr0030_constraint_dispatch::drop_constraint_is_published_and_stops_enforcing` | ADR 0030 slice 9: public `DROP CONSTRAINT` succeeds and immediately stops enforcing (duplicate now admitted) |
| `adr0030_constraint_drop_lifecycle::drop_constraint_releases_committed_values` | ADR 0030 slice 9: DROP frees committed values; after drain the value is unconstrained |
| `adr0030_constraint_drop_lifecycle::drop_then_recreate_same_name_different_label` | ADR 0030 slice 9: same-name re-CREATE → `Conflict` while `Dropping`, succeeds after `Removed` |
| `adr0030_constraint_drop_lifecycle::dropping_constraint_admits_new_inserts_but_blocks_recreate` | ADR 0030 slice 9: `Dropping` admits new INSERTs unconstrained but tombstones same-name re-CREATE |
| `adr0030_constraint_drop_lifecycle::drop_during_in_flight_insert` | ADR 0030 slice 9: DROP drains a held (`Reserved`) reservation; the dropped name is reusable |
| `adr0030_constraint_drop_lifecycle::recreate_blocked_until_pending_effect_drained` | ADR 0030 slice 9: completion gate keeps `Dropping` while a pinned effect remains ("no reservations" is insufficient) |
| `adr0030_constraint_drop_lifecycle::drop_survives_upgrade` | ADR 0030 slice 9: the `Dropping → Removed` lifecycle converges across a canister upgrade |
| `adr0030_constraint_drop_lifecycle::drop_does_not_disable_unrelated_constraints` | ADR 0030 slice 9: dropping one constraint does not weaken an unrelated one |
| `adr0030_uniqueness_shard_local_fast_path::shard_local_global_enforces_and_frees_on_delete` | ADR 0030 slice 10: single-shard CREATE freezes to `ShardLocalGlobal`; local-table enforces (duplicate → `UniquenessViolation`) and a DELETE frees the value by owner match |
| `adr0030_uniqueness_shard_local_fast_path::shard_local_global_drop_drains_local_table_and_allows_recreate` | ADR 0030 slice 10: `DROP CONSTRAINT` holds `Dropping` until the owning shard's local table drains, then the name is reusable |
| `adr0030_uniqueness_shard_local_fast_path::shard_local_global_survives_upgrade` | ADR 0030 slice 10: the `ShardLocalGlobal` local unique table persists across a canister upgrade |
