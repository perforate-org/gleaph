---
name: rust-workflow
description: Format, type-check, clippy, test, benchmark, and completion reporting for Rust changes.
---

# Rust Workflow

Use this skill whenever modifying Rust code in this repository.

## Goal

Keep Rust changes formatted, type-correct, lint-clean, tested, and aligned with
the repository's architecture before considering the task complete.

## Development vs Completion

Prefer targeted checks during development and completion. Broaden only when the plan, user, or repository
contract explicitly names a broader final gate.

Use `-p <crate>` and focused test filters for the affected ownership surface. Full-workspace validation,
the full PocketIC suite, and unfiltered canbench are not default completion requirements.

## Required Checks

After meaningful Rust code changes, run the affected scope in order:

1. Formatting
2. Type check
3. Clippy
4. Tests
5. Relevant benchmarks
6. If canbench results changed intentionally, update persisted results

```sh
cargo fmt --all -- --check
cargo check -p <affected-crate> --tests
cargo clippy -p <affected-crate> --all-targets -- -D warnings
cargo test -p <affected-crate> --lib <focused-filter>
# benchmarks: see the `benchmark` skill
```

Do not run all targets/features when the change does not cross them. Do not run a compile-only command
immediately before the same runtime target solely for reassurance. For independent validation, obey the exact
command allowlist and non-mutation rules in `cost-aware-validation`.

## Fix Workflow (check / clippy)

When `cargo check` or `cargo clippy` reports errors or warnings:

1. Read the diagnostic and identify the root cause.
   Run the command with its full diagnostic output. Do not pipe compiler/test output through
   `head`, `tail`, or another truncating filter while diagnosing or reporting a gate; truncation can
   hide the primary error, help text, test summary, or a later independent failure. If output is too
   large, save the complete transcript and inspect targeted ranges without discarding it.
2. Apply the smallest correct fix.
3. Prefer manual fixes when ownership, lifetimes, APIs, architecture, error handling, or domain logic are involved.
4. Use automated fixes only for mechanical changes:

```sh
cargo clippy --workspace --all-targets --all-features --fix --allow-dirty --allow-staged
```

5. Re-run the affected command after each fix.
6. Continue until diagnostics are resolved.
7. Do not silence warnings with `#[allow(...)]` unless there is a documented architectural reason.

### Unused variables and arguments

When `unused variable` or `unused argument` appears:

1. Decide whether the binding is actually required for the API, trait contract, forward compatibility, or a future hook.
2. If it is not required, remove it from the signature or binding and update call sites.
3. Do not default to renaming `name` to `_name` just to satisfy the compiler.
4. Use a bare `_` parameter name only when the parameter must stay in the signature but is intentionally ignored (for example a trait method or public stub).
5. Prefer deleting dead `let` bindings entirely rather than assigning them to `_`-prefixed locals.

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
