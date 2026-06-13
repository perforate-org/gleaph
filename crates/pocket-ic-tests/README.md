# PocketIC federation tests

## Prerequisites

- PocketIC server: `.pocket-ic/pocket-ic` (fetched at build time if missing or version mismatch). `POCKET_IC_BIN` may override it only when the binary reports the same `pocket-ic-server` version as the `pocket-ic` crate dependency; a mismatched override is ignored (stale shell exports otherwise cause composite-query timeouts).
- `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`

## Run

```bash
# Router + index placement smoke (default build)
cargo test -p gleaph-pocket-ic-tests router_registers_shards -- --nocapture

# All tests
cargo test -p gleaph-pocket-ic-tests -- --nocapture
```

## Status

| Test                                                                    | Coverage                                                |
| ----------------------------------------------------------------------- | ------------------------------------------------------- |
| `router_placement::router_registers_shards_and_commits_active_placement` | `admin_register_shard`, index owner map, active placement |
| `graph_seed_dispatch::graph_execute_plan_query_skips_index_scan_with_seed_bindings` | Federated graph `execute_plan_query` + `seed_bindings_blob` |
| `graph_seed_dispatch::graph_execute_plan_query_rejects_index_scan_without_seeds` | Federated graph rejects bare `IndexScan` without router seeds |
| `router_gql_query::router_gql_query_node_scan_on_single_shard` | Router `gql_query` composite dispatch on a single registered shard |
| `router_gql_query::standalone_e2e_insert_commits_placement_and_global_id` | Standalone `e2e_insert_vertex` → `GlobalVertexId` + router `resolve_placement` |
| `router_gql_query::standalone_gql_query_index_seeded_property_eq` | Single-shard router `gql_query` with `CREATE INDEX` DDL + indexed property equality anchor |
| `router_gql_query::standalone_gql_query_returns_element_id_bytes` | Router `gql_query` returns encoded `ELEMENT_ID` bytes via `rows_blob` |
| `router_gql_query::federated_gql_query_index_seeded_routes_to_hit_shard_only` | Multi-shard `gql_query` with `CREATE INDEX` DDL; slices index hits to the matching shard |
| `router_gql_query::federated_gql_query_index_seeded_merges_across_shards` | Multi-shard `gql_query` with `CREATE INDEX` DDL; merges rows when both shards match the anchor |
| `router_gql_query::standalone_drop_index_property_eq_still_queries_via_scan` | `DROP INDEX` on single shard; property equality still works via scan |
| `router_gql_query::federated_drop_index_property_eq_loses_federated_anchor` | `DROP INDEX` on multi-shard; indexed equality query fails without anchor |
| `router_gql_query::drop_index_if_exists_is_idempotent` | `DROP INDEX … IF EXISTS` twice succeeds |
| `router_gql_query::drop_index_without_if_exists_errors_when_missing` | Bare `DROP INDEX` on missing name returns `NotFound` |
