---
name: cost-aware-validation
description: Design and review Rust tests, PocketIC E2E fixtures, validation loops, and canbench benchmarks for high signal at bounded compile and runtime cost. Use when adding or consolidating tests, choosing unit versus E2E coverage, planning validation commands, reducing repeated federation setup, creating performance benchmarks, updating canbench artifacts, or investigating slow test/build workflows.
---

# Cost-Aware Validation

Optimize validation cost only after preserving the behavioral contract. A faster suite that permits
false passes is a regression.

## Strict validation-only operating mode

When assigned independent validation, validate the existing worktree; do not repair it.

- If the prompt supplies an ordered command allowlist, run only those commands, once each, in that order.
- Stop at the first failure unless the prompt explicitly asks to continue collecting independent results.
- Do not edit, format-write, generate, stage, restore, delete, or commit files. Use `cargo fmt --check`, not
  formatting that mutates the tree.
- Do not invent broader commands, cold target directories, GUI fallbacks, external terminals, background
  jobs, or delegated validation.
- Do not invoke `ps`, `pgrep`, `/ps`, `/stop`, `pkill`, or `kill`. If the active foreground command exceeds
  its budget, interrupt only that command through the current tool/session and report it incomplete.
- Do not retry a failed or timed-out command through another wrapper. Record the exact failure and stop.
- Do not diagnose by changing source. Return the evidence to the implementation/review owner.

Before the first command, restate the allowlist and total budget internally. After the last completed command,
report one row per command as passed, failed, incomplete, or not run. A notification is not proof of success;
the terminal result is.

## Choose the Cheapest Owning Layer

Place each assertion at the layer that owns the invariant:

- pure encoding, bounds, state transitions, and planner decisions: unit tests;
- crate API and storage reopen behavior: crate integration tests;
- Router/Graph/Index wiring, Candid, upgrade, timers, fault injection, and cross-shard behavior:
  PocketIC E2E;
- instruction count or scaling behavior: canbench.

Keep one E2E path for a real canister boundary, but move combinatorial edge cases to the owning unit
layer. Do not duplicate the same predicate matrix at every layer.

## Test Fixture Budget

Before adding a test, inventory its expensive setup calls and ask whether an existing fixture family
can express the contract safely.

Record the actual expensive-constructor and install-call count before and after the change. Test
function count is not a substitute: one E2E test containing several named scenarios still has the
fixture budget of the constructors and installs it executes. Count Rust tests, PocketIC constructors,
and canister/federation installation helpers separately.

For PocketIC:

- Treat every `install_federation()` or `install_single_shard_federation()` as a large fixed cost.
- Treat direct `PocketIc` construction and lower-level canister installation helpers as the same
  class of fixed cost; do not hide one-bootstrap-per-scenario behind a shared wrapper or one test.
- Group compatible contracts into one lifecycle test with named scenario helpers.
- Keep separate `#[test]` fixtures when metric, schema, topology, failure injection, upgrade state, or
  irreversible mutation would contaminate another scenario.
- Never share a live `PocketIc` globally across tests.
- Prefer the smallest topology that owns the contract: single shard for local GQL execution; full
  federation only for routing, all-shard gates, or cross-shard behavior.
- Do not use `--test-threads=1` to hide fixture interference. Fix isolation instead.

When consolidating, map every former test to a named scenario and apply
`adversarial-test-review`. Exact identities, values, ordering, path shapes, and post-error state are
more useful than row counts alone.

## Focused Rust Loop

Use the smallest affected scope during implementation:

```sh
cargo fmt --all -- --check
git diff --check
cargo check -p <crate> --tests
cargo clippy -p <crate> --test <target> -- -D warnings
cargo test -p <crate> --test <target> [filter]
```

Avoid redundant compilation:

- Choose `cargo check` for an early compile-only iteration or focused `cargo clippy` for closure;
  do not routinely run both against the same unchanged target. For an integration test, prefer
  `cargo clippy -p <crate> --test <target> -- -D warnings`; add `--all-targets` or `--all-features`
  only when the change actually crosses those surfaces.
- Do not run `--no-run` immediately before the same runtime target when the runtime command will
  compile it anyway.
- Do not run check, clippy, and test repeatedly after every mechanical edit; start with format and the
  narrowest compiler/test signal, then run the completion sequence once.
- Reuse the normal target directory during ordinary work. Never `cargo clean` a shared target.
- Use isolated `CARGO_TARGET_DIR`s only for clean timing comparisons or when a stale lock blocks the
  shared target; record that the measurement is cold.
- Avoid workspace-wide, all-target, or all-feature commands unless the affected contract or final
  gate requires them.

Full workspace tests, the full PocketIC suite, and unfiltered canbench are explicit final gates, not
background reassurance.

## Long-Running Budget

- Prefer one affected PocketIC target and one focused benchmark pattern.
- Stop a command after five minutes without meaningful output and stop the active validation turn
  after ten minutes.
- Cancel only the currently owned foreground command when it exceeds budget. Do not enumerate or kill
  unrelated cargo/rustc/PocketIC processes from a validation-only pane.
- A timed-out, background, delegated, or `--no-run` command is not a runtime pass.
- Report the exact command, last observed state, and whether work is compiled, tested, deferred, or
  incomplete.
- Do not immediately replace one timed-out command with another cold build or long fallback.

## Benchmark Fitness

Benchmark one contract at a time:

- Keep setup, interning, fixture construction, membership assertions, and sanity checks outside the
  measured closure.
- Assert the benchmark result once before measurement so a fast wrong path cannot look good.
- Hold input cardinality, survivor count, page size, and data density fixed when comparing scaling by
  arm count or another independent variable.
- Put adversarial sparse/scattered/fallback behavior in a separate benchmark series; do not mix it
  into the dense baseline.
- Use arithmetic widths that cannot overflow synthetic ids or sizes.
- Explain fixture changes before interpreting instruction deltas as regressions.

During development run `canbench <pattern>` from the affected crate. When intentionally updating the
final artifact, run unfiltered `canbench --persist` in every affected crate. Never use a pattern with
`--persist`; it can truncate or stale the artifact. Revert unrelated remeasurement noise instead of
landing it as product impact.

## Review Gate

Before approval, answer:

1. What invariant is uniquely protected by each test or benchmark?
2. Could a cheaper owning-layer test provide the same signal?
3. Does each expensive bootstrap cover several compatible contracts without state masking?
4. Can a wrong implementation still satisfy the assertions?
5. Does the benchmark vary only the dimension it claims to measure?
6. Which commands actually completed, and which were skipped or deferred?
7. Did validation leave background processes, partial artifacts, or ignored plan statuses behind?

Reject changes that add heavyweight setup without a boundary-level reason, duplicate an existing
contract without independent signal, weaken observability during consolidation, or persist an
uncontrolled benchmark fixture.

## Completion Report

Report:

- contracts added, preserved, consolidated, or intentionally deferred;
- bootstrap/test-binary count before and after when relevant;
- exact focused commands and results;
- benchmark fixture shape and measured variable;
- persisted artifact status;
- elapsed time or timeout state for long commands;
- remaining broad gates not run.
