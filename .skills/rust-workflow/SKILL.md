# Rust Workflow

Use this skill whenever modifying Rust code in this repository.

## Goal

Keep Rust changes formatted, type-correct, lint-clean, tested, and aligned with
the repository's architecture before considering the task complete.

## Development vs Completion

Prefer targeted checks during development, then broader checks before completion.

During iteration, you may use `-p <crate>` for faster feedback. Final verification
should use the full workspace unless there is a known unrelated workspace-wide failure.

## Required Checks

After meaningful Rust code changes, run in order:

1. Formatting
2. Type check
3. Clippy
4. Tests
5. Relevant benchmarks
6. If canbench results changed intentionally, update persisted results

```sh
cargo fmt --all
cargo check --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
# benchmarks: see the `benchmark` skill
```

## Fix Workflow (check / clippy)

When `cargo check` or `cargo clippy` reports errors or warnings:

1. Read the diagnostic and identify the root cause.
2. Apply the smallest correct fix.
3. Prefer manual fixes when ownership, lifetimes, APIs, architecture, error handling, or domain logic are involved.
4. Use automated fixes only for mechanical changes:

```sh
cargo clippy --workspace --all-targets --all-features --fix --allow-dirty --allow-staged
```

5. Re-run the affected command after each fix.
6. Continue until diagnostics are resolved.
7. Do not silence warnings with `#[allow(...)]` unless there is a documented architectural reason.

## Repeated Failure Rule

Do not repeatedly guess at fixes.

If the same error, warning, or failure pattern appears twice:

1. Stop making speculative changes.
2. Investigate the diagnostic, relevant documentation, and existing repository patterns.
3. Consider multiple possible solutions.
4. Choose the smallest solution that preserves the intended design.

## Architecture Preservation

Compiler and lint compliance must not come at the expense of architecture.

In particular:

- `gleaph-gql` and `gleaph-gql-planner` must remain general-purpose GQL crates.
- Gleaph-specific, Internet Computer-specific, canister-specific, storage-specific, or application-specific logic must not leak into those crates.
- Router, graph, property index, and vector index responsibilities must remain separated.
- Avoid introducing unnecessary heap allocations, query-time overhead, or architectural shortcuts solely to satisfy a lint.

For Gleaph boundary details, also consult the `gleaph-architecture` skill.

## Before Completion

A Rust task is not complete until:

- `cargo fmt`, `cargo check`, and `cargo clippy` succeed for the affected scope.
- Tests pass for the affected scope.
- Relevant benchmarks were run; persisted canbench results updated if intentional.
- Any remaining failures are confirmed pre-existing or unrelated to the current change.
- The completion report includes:
  - Commands executed
  - Results of each command
  - Any remaining known issues

## Mandatory Verification

Never claim a Rust task is complete without actually running the required verification commands.

Do not assume code compiles based solely on reasoning. Always verify by executing the commands.
