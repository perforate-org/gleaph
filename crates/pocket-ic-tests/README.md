# PocketIC federation tests

## Prerequisites

- PocketIC server: `.pocket-ic/pocket-ic` (fetched at build time if missing or version mismatch), or override with `POCKET_IC_BIN` at test runtime
- `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`

## Run

```bash
# Router + index placement smoke (default build)
cargo test -p gleaph-pocket-ic-tests router_registers_shards -- --nocapture

# Full graph migration E2E (builds graph WASM with pocket-ic-e2e)
POCKET_IC_BUILD_GRAPH=1 cargo test -p gleaph-pocket-ic-tests incremental_migration -- --nocapture

# All tests including graph E2E
POCKET_IC_BUILD_GRAPH=1 cargo test -p gleaph-pocket-ic-tests -- --nocapture
```

## Status

| Test                                                                     | Coverage                                                                                              |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| `router_placement::router_registers_shards_and_runs_placement_migration` | `admin_register_shard`, index owner map, `begin_vertex_migration` / `finish_vertex_migration`         |
| `incremental_migration::incremental_migration_copy_cutover_and_prune`    | Multi-canister copy, cutover, stub prune maintenance, incoming `federated_expand` via forwarding stub |
