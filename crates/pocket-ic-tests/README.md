# PocketIC federation tests

## Prerequisites

- PocketIC server: `.pocket-ic/pocket-ic` (fetched at build time if missing or version mismatch), or override with `POCKET_IC_BIN` at test runtime
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
