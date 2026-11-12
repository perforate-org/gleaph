# General routing: graph-owner authority implementation (2026-11-12)

Status: implemented and measured for ADR 0115 slices 1–2.

## Implemented

- `rbac` owns the two ADR 0115 predicates:
  - `authorize_prepared_graph_change(caller, graph_id, existing_graph)`: global
    `PREPARE_REGISTER`, or tenant of the bound graph; replacing a name bound to another graph
    requires the global capability and returns `Conflict`.
  - `authorize_graph_routing_authority(caller, graph_id)`: Router controller or graph tenant.
- `prepare` now resolves and authorizes every operation **before any mutation**; the batch remains
  all-or-nothing. `drop_prepared` loads the record first and authorizes against its bound graph.
  The cross-graph name takeover is a typed `Conflict` rather than a wrapped invalid argument.
- `general_routing_profile`, `admin_set_general_routing_profile` and
  `admin_general_route_raw` now use the tenant/controller arm instead of controller-only.
  `admin_general_route_raw` is a composite query forwarding the inference canister query, so
  calibration does not spend one update per item; the tokenizer token-count endpoint is query-only.
  `preview_route_general` is also a composite query, tenant/controller-only, and never consumes
  quota; the quota-charged production route remains an update.
- The old global-only `authorize_prepared_catalog_change` helper is deleted; the capability bit is
  exposed through `has_global_prepared_capability` for the remaining call sites/tests.

## Verification

```sh
cargo test -p gleaph-router prepared::tests
# 17 passed, including the new graph-tenant home registration/cross-graph conflict test

POCKET_IC_BIN=<pocket-ic 15> \
  cargo test -p gleaph-pocket-ic-tests --test general_route_authority -- --nocapture
# 1 passed: graph admin (not Router controller) sets/reads its profile;
# non-tenant profile read/write and raw calibration are Forbidden (raw via query_call)

# general_router_route also exercises positive composite-query raw and preview routes.
```

Runtime checks were rebuilt from the changed Router wasm by the PocketIC harness.

## Caveats

- The default federation graph name (`gleaph.pocket_ic`) contains a dot and cannot be referenced by
  top-level `USE`, so the tenant **prepared registration** end-to-end test is a Router unit test
  with a home graph; profile/raw authority is the PocketIC E2E.
- `list_prepared`/`get_prepared` remain operator surfaces; prepared names remain Router-global
  (ADR 0063). Graph-scoped names and a durable audit history are deferred.
- The profile write has no new audit ledger in this slice.
- The owner workflow documentation (`gleaph prepared apply`) is not updated yet.

## Owner-facing CLI (slice 3)

`gleaph route profile --canister <ROUTER> --graph <NAME> [--json]` reads the profile through the
Router `general_routing_profile` query. `--set-threshold-bps <0..=10000> --policy-version <N>`
writes it through `admin_set_general_routing_profile`; the Router still enforces controller/tenant
authority. The CLI validates the bps range before connecting. Calibration remains
`gleaph ask --calibrate` (recommendation only); the documented admin handoff is
`gleaph route profile --set-threshold-bps <recommended> --policy-version <N>`.

Verification:

```sh
cargo test -p gleaph-sdk --features routing
# mock transport covers routing_profile/set_routing_profile, raw route and general route
cargo test -p gleaph-cli
# route validation + existing ask tests
```
