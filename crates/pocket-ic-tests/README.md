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
| `graph_seed_dispatch::graph_execute_plan_query_skips_index_scan_with_seed_bindings` | Federated graph `execute_plan_query` + `seed_bindings_blob` |
| `graph_seed_dispatch::graph_execute_plan_query_rejects_index_scan_without_seeds` | Federated graph rejects bare `IndexScan` without router seeds |
| `router_gql_query::router_gql_query_node_scan_on_single_shard` | Router `gql_query` composite dispatch on a single registered shard |
| `router_gql_query::standalone_e2e_insert_assigns_global_id` | Standalone `e2e_insert_vertex` → `GlobalVertexId` from graph routing |
| `router_gql_query::standalone_gql_query_index_seeded_property_eq` | Single-shard router `gql_query` with `CREATE INDEX` DDL + indexed property equality anchor |
| `router_gql_query::standalone_gql_query_edge_index_seeded_property_eq` | Single-shard router `gql_query` with edge `CREATE INDEX` DDL + indexed edge property equality anchor |
| `router_gql_query::standalone_gql_query_returns_element_id_bytes` | Router `gql_query` returns encoded `ELEMENT_ID` bytes via `rows_blob` |
| `router_gql_query::standalone_gql_query_returns_relationship_rows_for_knowledge_map_adapter` | Router `gql_query` returns relationship row material needed by the knowledge-map adapter |
| `router_gql_query::federated_gql_query_index_seeded_routes_to_hit_shard_only` | Multi-shard `gql_query` with `CREATE INDEX` DDL; slices index hits to the matching shard |
| `router_gql_query::federated_gql_query_index_seeded_merges_across_shards` | Multi-shard `gql_query` with `CREATE INDEX` DDL; merges rows when both shards match the anchor |
| `router_gql_query::standalone_drop_index_property_eq_still_queries_via_scan` | `DROP INDEX` on single shard; vertex property equality still works via scan |
| `router_gql_query::standalone_drop_edge_index_property_eq_still_queries_via_scan` | `DROP INDEX` on single shard; edge property equality still works via scan |
| `router_gql_query::federated_drop_index_property_eq_loses_federated_anchor` | `DROP INDEX` on multi-shard; indexed vertex equality query fails without anchor |
| `router_gql_query::federated_drop_edge_index_property_eq_loses_federated_anchor` | `DROP INDEX` on multi-shard; indexed edge equality query fails without anchor |
| `router_gql_query::drop_index_if_exists_is_idempotent` | `DROP INDEX … IF EXISTS` twice succeeds |
| `router_gql_query::drop_index_without_if_exists_errors_when_missing` | Bare `DROP INDEX` on missing name returns `NotFound` |
| `router_graph_type_catalog::catalog_create_graph_type_returns_zero_rows` | ADR 0013: `CREATE GRAPH TYPE` on router stable catalog |
| `router_graph_type_catalog::catalog_typed_binding_persists_across_calls` | ADR 0013: `CREATE GRAPH … TYPED` + `gql_query` after catalog DDL |
| `router_graph_type_catalog::catalog_create_graph_unregistered_name_rejected` | ADR 0013: `CREATE GRAPH` without federation registration → `NotFound` |
| `router_graph_type_catalog::catalog_drop_graph_type_cascades_typed_binding` | ADR 0013: `DROP GRAPH TYPE` removes type; rebinding fails |
| `router_graph_type_catalog::catalog_typed_schema_rejects_undirected_match_on_directed_edge` | ADR 0013: typed schema rejects `MATCH` edge direction mismatch at ingress |
