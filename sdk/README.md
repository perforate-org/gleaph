# SDK Workspace

`sdk/` contains publishable SDK packages for Gleaph. The directory is split by consumer so that client, canister, and (future) admin surfaces do not leak into one another.

## Layout

```
sdk/
├── README.md
├── client/
│   └── js/          # @gleaph/sdk — browser/Node client SDK
└── canister/
    └── rust/        # gleaph-cdk — ic-cdk-based helpers for canister internals
```

`sdk/admin/{js,rust}` is planned but not implemented yet. It will hold management surfaces such as prepared-registration, graph administration, and vector-index lifecycle. Those surfaces are intentionally kept out of `@gleaph/sdk` and `gleaph-cdk`.

## `@gleaph/sdk`

Location: `sdk/client/js`

JS/TS-facing client runtime with typed DTOs for the graph canister API, helpers for `USE GRAPH` pushdown capability and warning handling, and the IC transport / prepared-query runtime.

Package name remains `@gleaph/sdk` for now. The Router L1 operation surface is represented by the
breaking `gql_query`, `gql_mutate`, `prepared_query`, `prepared_mutate`, and atomic-insert
contracts; superseded operation names are not retained as aliases.

## `gleaph-cdk`

Location: `sdk/canister/rust`

Rust canister SDK seeded with helpers used by application canisters that delegate fixed read scenarios to the Gleaph Router. The initial API is intentionally small:

- `GqlValue`, `GqlRecord`, `GqlParams`, and `GqlRow` — shared logical GQL value types for dynamic GQL and prepared operations.
- `GqlFloat256` — a Serde-compatible wrapper whose representation is exactly 32 little-endian bytes. `GqlFloat128` is available with the `nightly-f128` feature and uses 16 little-endian bytes.
- `encode_gql_params(params)` — compact-binary encoding for ordered logical GQL parameters.
- `call_gql_query::<R>(canister_id, query, params, read_mode)` — bounded-wait inter-canister call to the Router's dynamic `gql_query` endpoint with explicit read consistency.
- encode_prepared_query_args(name, params, read_mode) - Candid-encode the `(String, Vec<u8>, Option<Vec<PreparedSortSpec>>, ReadMode)` tuple used by Router `prepared_query`.
- call_prepared_query::<R>(canister_id, name, params, read_mode) - bounded-wait inter-canister call to `prepared_query` with structured reject/decode errors.
- call_prepared_mutate::<R>(canister_id, name, params, client_mutation_key) - bounded-wait inter-canister call to the idempotent `prepared_mutate` update.
- `GleaphClient` — canister-id-bound wrapper for dynamic GQL and prepared operations.

Admin/management operations are not included; they belong in `sdk/admin/rust` when that slice lands.

## Status

- Client SDK moved to `sdk/client/js` and workspace references updated.
- `gleaph-cdk` crate created at `sdk/canister/rust` and adopted by `crates/social-demo-gateway`.
- Admin SDK boundary documented above but not implemented.

## Build-from-source expectation

`sdk/client/js/dist/` is a build artifact produced by `pnpm --filter @gleaph/sdk run build` (or `pnpm sdk:build`). It is intentionally not tracked in git.

For local workspace consumers, run the root `install:all` script after a fresh clone:

```sh
pnpm install:all
# equivalent to: vp install && pnpm sdk:build
```

Or, after a plain `pnpm install`, build the SDK explicitly:

```sh
pnpm sdk:build
```

The SDK package also declares a `prepare` script so that `dist/` is rebuilt before `pnpm publish` and for consumers that install `@gleaph/sdk` as a git dependency.

Do not commit `dist/` files; the root `.gitignore` and `sdk/client/js/.gitignore` block them.
