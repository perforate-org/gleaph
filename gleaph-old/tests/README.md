# gleaph-tests

Workspace test crate for Gleaph.

This crate hosts cross-crate tests that are easier to maintain outside individual library/canister crates.

## What Is Tested

- PMA smoke behavior on the host (`tests/src/pma_unit.rs`)
- PocketIC-based end-to-end canister flows (`tests/src/e2e.rs`)
- Graph canister CRUD basics
- Bulk insert + stats checks
- Graph canister upgrade data preservation
- Registry canister graph creation
- Registry upgrade compatibility using legacy snapshot fixtures

## Test Modes

Fast/default tests:

```bash
cargo test -p gleaph-tests
```

Ignored integration tests (require local wasm artifacts and PocketIC runtime):

```bash
cargo build -p gleaph-graph -p gleaph-registry --target wasm32-unknown-unknown
cargo test -p gleaph-tests -- --ignored
```

For registry legacy-upgrade tests, also build the legacy fixture canister:

```bash
cargo build -p gleaph-registry-legacy-fixture --target wasm32-unknown-unknown
```

## Why A Separate Test Crate?

It keeps:

- host-native PMA checks
- wasm artifact-driven integration tests
- compatibility fixtures

in one place without adding heavy dev-dependencies to production crates.
