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

Package name remains `@gleaph/sdk` for now. No public API changes were made during this reorganization.

## `gleaph-cdk`

Location: `sdk/canister/rust`

Rust canister SDK seeded with helpers used by application canisters that delegate fixed read scenarios to the Gleaph Router. The initial API is intentionally small:

- `encode_prepared_query_args(name, params)` — Candid-encode the `(String, Vec<u8>)` argument tuple used by Router prepared queries.
- `call_prepared_query::<R>(canister_id, name, params)` — bounded-wait inter-canister call to `prepared_execute_query` with structured reject/decode errors.
- `PreparedQueryClient` — thin canister-id-bound wrapper around `call_prepared_query`.

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
