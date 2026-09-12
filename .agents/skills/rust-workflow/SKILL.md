---
name: rust-workflow
description: Run scoped Rust checks, PocketIC tests, and canbench with bounded execution and explicit completion evidence.
---

# Rust Workflow

Use for Rust implementation or assigned Rust, PocketIC, or canbench validation. This file owns
command selection, execution budgets, independent-validation restrictions, and completion evidence.
Reading it during a read-only review does not authorize validation execution.

## Scope and Command Selection

- Select the affected crates, targets, and features. Use `-p <crate>` and target selectors such as
  `--lib`, `--test <target>`, or `--bin <target>`; include affected test targets when linting tests.
  Use `--all-targets` or `--all-features` only for affected surfaces or explicitly required gates.
- Full-workspace tests, the full PocketIC suite, and unfiltered canbench are explicit final gates,
  not default reassurance. Final benchmark artifact updates require the full runs described below.
- Use ordinary `cargo test` for PocketIC/canbench-related test targets and doctest-sensitive paths.
  Use `cargo nextest run` only where suite compatibility is known; include
  `cargo test -p <crate> --doc` when Rust documentation examples are affected.
- Reuse the normal target directory. Never `cargo clean` a shared target. Isolated
  `CARGO_TARGET_DIR`s are for deliberate cold timing comparisons or a blocking stale lock, not
  routine reassurance; report cold measurements. Independent validation has stricter rules below.
- Prose- or instruction-only changes need content, reference, and `git diff --check` verification,
  not Rust builds, tests, or benchmarks. Changes to executable examples need their owning checks.

For test placement and fixture design, use [cost-aware-validation](../cost-aware-validation/SKILL.md).
For benchmark measurement and regression decisions, use [benchmark](../benchmark/SKILL.md).

## Implementation Checks

During iteration, start with the narrowest useful compiler or test signal. Use
`cargo check -p <crate> <target-options>` for an early compile-only check, or run the affected test
when runtime behavior is the question. Do not repeat the whole completion sequence after every
mechanical edit.

After meaningful Rust changes, finish with formatting, scoped clippy, affected tests, and relevant
benchmarks. Choose target options for the affected code rather than broadening every command:

```sh
cargo fmt --all -- --check
git diff --check
cargo clippy -p <crate> <target-options> -- -D warnings
cargo test -p <crate> <target-options> [filter]
```

Clippy also type-checks its selected targets. A separate `cargo check` is not a completion
requirement for the same unchanged target/features; add it only for a distinct uncovered scope,
a diagnostic need, or an explicit gate. Likewise, do not run `--no-run` immediately before the
same runtime target merely for reassurance. Confirm that test filters actually ran the intended tests.

## PocketIC and Canbench

Use direct commands rather than `just` for ordinary execution:

- Focused PocketIC: `cargo test -p gleaph-pocket-ic-tests --test <test-name> [filter]`.
- Full PocketIC, only for an explicitly required full-suite gate: `cargo test -p gleaph-pocket-ic-tests`.
- Focused canbench: run `canbench <pattern>` from the affected crate.
- When intentionally updating final benchmark artifacts, run unfiltered `canbench --persist` from
  every affected crate with canbench benchmarks. Never combine a pattern with `--persist`; partial
  persistence can truncate or stale `canbench_results.yml`. Keep each affected artifact complete
  and exclude unrelated remeasurement noise from the patch.

A code, build, or test failure is not grounds for retrying through another wrapper. Outside independent
validation, use [pocketic-just-fallback](../pocketic-just-fallback/SKILL.md) only for a known macOS
editor-hosted sandbox process-chain failure. A timeout alone is not evidence of that failure.
Terminal.app delegation does not establish completion; apply the execution budget and report evidence.

## Execution Budget

- Stop a command after five minutes without meaningful output. Limit total active observation of
  long-running validation to ten minutes per agent turn; do not reset the budget by switching commands.
- When a limit is reached, interrupt only the owned foreground command through its tool/session when
  safe. Do not enumerate or kill unrelated cargo, rustc, or PocketIC processes.
- Record the exact command and last observed state. Do not replace a timed-out command with another
  long synchronous fallback, delegated wait, or cold build. Continue other useful work or report the
  remaining validation as incomplete or deferred.

## Independent Validation

When assigned independent validation, validate the existing worktree; do not repair it. These
restrictions take precedence over the implementation repair workflow:

- If an ordered command allowlist is supplied, run only those commands, once each, in that order.
  Otherwise select the narrow affected scope under this workflow before starting.
- Stop at the first failure unless explicitly asked to continue collecting independent results.
- Do not edit, format-write, generate source or benchmark artifacts, stage, restore, delete, or commit
  files. Formatting is check-only; do not use automatic fixes or persist benchmark results.
- Do not invent broader commands, cold target directories, GUI fallbacks, external terminals,
  background jobs, or delegated validation. Do not retry through another wrapper or change source
  to diagnose a failure.
- Do not invoke `ps`, `pgrep`, `/ps`, `/stop`, `pkill`, or `kill`. If the active foreground command
  exceeds its budget, interrupt only that command through the current tool/session.

Before execution, identify the allowlist or selected commands and the applicable budget. Return
results to the implementation/review owner using the completion evidence format below.

## Diagnostic Repairs During Implementation

When a compiler, clippy, or test diagnostic fails:

1. Read the full diagnostic and identify the cause. Do not pipe gate output through `head`, `tail`,
   or another truncating filter. If output is too large, save the complete transcript and inspect
   targeted ranges without discarding it.
2. Apply the smallest correct fix. Prefer manual changes for ownership, lifetimes, APIs, architecture,
   error handling, or domain logic. Automatic fixes are only for mechanical changes within the
   selected scope; inspect their diff rather than using workspace-wide repair commands.
3. Re-run the affected command after the fix. Resolve failures caused by the change rather than
   suppressing diagnostics. Use `#[allow(...)]` only with a documented architectural reason.

For repeated failures, follow [AGENTS.md — Error Handling](../../../AGENTS.md#error-handling).
Preserve the repository's [boundary contracts](../../../AGENTS.md#repository-integrity) while fixing
errors. Avoid unnecessary heap allocations, query-time overhead, or architectural shortcuts solely
to satisfy a lint; consult `gleaph-architecture` when a fix affects Gleaph boundaries.

### Unused Variables and Arguments

1. Decide whether the binding is actually required for the API, trait contract, forward compatibility,
   or a future hook.
2. If it is not required, remove it from the signature or binding and update call sites.
3. Do not default to renaming `name` to `_name` just to satisfy the compiler.
4. Use a bare `_` parameter name only when the parameter must stay in the signature but is intentionally
   ignored (for example a trait method or public stub).
5. Prefer deleting dead `let` bindings entirely rather than assigning them to `_`-prefixed locals.

## Completion Evidence

Report one result per required or assigned command: **passed**, **failed**, **incomplete**, or
**not run/deferred**, with the exact command and relevant scope. Include the reason for deferred
checks, remaining risks, and the last observed state for interrupted commands. Include the working
directory when it affects execution, elapsed time for long runs, and persisted-artifact status for
benchmark updates. Distinguish failures confirmed pre-existing or unrelated from failures caused
by the change; neither is a passed gate.

A Rust implementation is fully validated only when its required scoped checks have completed
successfully. Build-only checks, clippy, `--no-run`, unfinished background/delegated processes, or
completion notifications are not runtime passes. Do not mark the task or final benchmark artifacts
fully validated while required runtime checks remain unfinished; report the outstanding work rather
than extending the execution budget to force completion.
