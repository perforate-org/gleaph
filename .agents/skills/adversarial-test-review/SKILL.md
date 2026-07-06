---
name: adversarial-test-review
description: Perform strictly read-only adversarial reviews of test additions, consolidations, refactors, and architecture-sensitive diffs against their plan and behavioral contracts. Use for independent diff review, PocketIC/E2E consolidation, lifecycle tests, fail-closed error paths, filtering/ranking/path selection, multi-source or multi-direction coverage, and any review where assertion strength or fixture ordering determines correctness. Never edit files, run tests/builds/benchmarks, manage processes, or open external terminals during this review mode.
---

# Adversarial Test Review

Review the actual diff independently. Do not approve from the implementation report alone.

## Strict read-only operating mode

Treat this skill as a low-freedom inspection procedure. The review owns findings, not fixes or
validation execution.

Allowed operations are limited to:

- reading the plan, instructions, source files, tests, documents, and existing terminal reports;
- `git status`, `git diff`, `git show`, `git log`, `git grep`, `rg`, `sed`, and equivalent read-only
  inspection;
- reporting findings or approval to the coordinating pane.

Do not:

- edit, format, generate, stage, restore, delete, or create any file;
- run `cargo test`, `cargo check`, `cargo clippy`, `cargo fmt`, canbench, PocketIC, or any build;
- start background jobs, use `/ps` or `/stop`, invoke `pkill`/`kill`, or inspect/manage processes;
- open Terminal.app, Ghostty, another pane, GUI application, or external validation environment;
- send fixes directly to an implementation pane unless the coordinating prompt explicitly requires
  that single notification after the review is complete;
- keep exploring after the finding set and verdict are determined.

If existing validation evidence is insufficient, report the exact missing evidence as a finding or
residual risk. Do not recreate it. If any forbidden operation is accidentally attempted, stop the
review immediately, disclose it, and do not continue with further tools.

Use one bounded inspection pass. Prefer at most 15 read-only shell calls and finish within five
minutes. Large diffs justify selective reads by owner and invariant; they do not justify executing
the code.

## Required-fix fidelity

On a fix re-review, copy every required correction from the coordinator into a checklist before
inspecting the diff. A required correction is blocking until the exact code or assertion satisfies it.
Do not downgrade an unmet required correction because the test name is broad, the old behavior also rejects,
or the remaining weakness seems minor. If the report observes that a required fix remains, the verdict cannot
be `APPROVE`. Make the finding status, non-blocking observations, and final verdict internally consistent.

When the coordinator explicitly requires a regression test for a discovered defect, replay that exact defect
against the new assertions. If restoring the defective statement or branch would still pass the test, the
required correction is unresolved and blocking (normally P1/P2), even when the production edit itself appears
correct. Queue emptiness, absence of an error, or canonical-state success does not prove that pre-existing or
derived work was delivered; require the observable named by the defect, such as exact calls, operations, values,
or durable journal entries.

## Inputs

Read, in this order:

1. The plan and its completion criteria.
2. The pre-change tests from `HEAD` or the review base.
3. The current diff and resulting test file.
4. The implementation report and validation evidence.

Before approving completion, compare the plan frontmatter status, every TODO/body checkbox, and the
final report. They must describe the same state. Also verify every cleanup claimed by the report is
actually absent from the current diff; a claimed cleanup that remains on disk is a finding.

Keep read-only reviews read-only. Do not rerun long tests, clippy, builds, or benchmarks unless explicitly assigned.

Timebox a focused test-only review to about three minutes. Make one plan/base/diff/report pass, then
finalize once traceability and counterexamples are complete. Do not repeatedly reread the whole diff,
probe unavailable process APIs, or keep exploring inherited non-blocking weaknesses after the verdict
is determined; list at most three useful non-blocking observations.

## Contract Traceability

Map every removed or changed contract to a current named scenario and assertion. For each plan criterion record:

- fixture/setup that establishes the precondition;
- operation under test;
- exact observable result;
- postcondition that detects forbidden state changes;
- assertion that would fail if the criterion were broken.

Missing traceability for a required criterion is a finding.

For PocketIC consolidation or additions, count test functions, `PocketIc` constructors, and
federation/canister installation calls independently. Reject one-bootstrap-per-compatible-scenario
even when the scenarios have been wrapped inside one test function.

## Mandatory Counterexample Pass

On a re-review, begin with the exact lines changed for each prior finding and compare the new assertions with
the promised error and postcondition. Treat broadened assertions (`A | B`, substring alternatives, or
"either error is acceptable") as unresolved when the corrected control-flow order should produce one precise
diagnostic. A fix must make the intended branch observable, not make both old and new behavior pass.

For every scenario, write at least one plausible wrong implementation that would still satisfy its assertions. Check these failure patterns explicitly:

- returns the expected error after mutating canonical or derived state;
- selects the wrong single member while preserving the expected row count;
- ignores a predicate, cost, ordering key, range arm, or filter source;
- exercises only one of several properties, sources, arms, labels, shards, or traversal directions;
- lets earlier fixture state mask a later scenario;
- uses identical/idempotent follow-up input that hides an unauthorized or partial write;
- claims an idempotent retry returns the stored first result, but retries with a byte-for-byte
  identical object; vary non-identity fields while preserving the idempotency key/fingerprint and
  require the first stored object and derived state to survive unchanged;
- bounds a prefix/range scan with a convenient maximum sentinel that is not a true successor of the
  prefix; include a valid key beyond the sentinel and a neighboring prefix in the counterexample;
- deduplicates or merges inputs so an advertised independent arm is never observed;
- reports success based only on compilation, `--no-run`, or an unfinished background process.

When a change adds an enum variant intended to share an existing variant's guards or conflict
semantics, perform a symmetric-variant audit: search every old-variant pattern, accessor, and helper
(for example `is_inline_scalar()` or `matches!(..., InlineScalar { .. })`). Require each call site to
include the new variant or document why that boundary is intentionally variant-specific. A test must
cover both mutation/DDL orders when either order can create conflicting state.

Strengthen observability with exact identities, values, ordering, path shape, call counts, or bounded state postconditions as appropriate. Row counts alone are insufficient when the wrong member can produce the same count.

Tests named for a specific failure mode must assert that exact error/path. Avoid disjunctive
assertions such as `A | B | C` when they allow an earlier guard to mask the branch named by the test.
If the targeted branch is mathematically unreachable under current bounds, state that proof and test
the reachable boundary instead of claiming direct coverage.

## Approval Gate

Connect every counterexample back to the plan:

- If a wrong implementation can pass an assertion for a required completion criterion, report a blocking finding. Approval is forbidden.
- If the weakness existed before the patch and the plan only promises preservation, label it non-blocking unless the plan explicitly requires stronger observability.
- If the plan explicitly promises independent observability, fail-closed behavior, or non-mutation, an inherited weak assertion does not excuse the gap.
- Do not turn optional strengthening into a blocker without a contract or risk justification.

Use `P1` for a missing core contract or false pass that defeats the slice objective, `P2` for a material but bounded coverage/process gap, and `P3` for minor accuracy or maintainability issues.

## Process and Evidence Audit

Verify:

- plan TODOs use repository vocabulary and match actual state;
- modified/untracked/ignored files agree with the report;
- commands actually ran with the claimed flags and results;
- forbidden flags such as `--test-threads=1` were not added;
- time budgets and deferred checks are reported honestly;
- no background validation process remains;
- test-only changes do not silently invalidate active docs or comments.

Do not duplicate expensive validation merely to confirm the implementer's report. Inspect the transcript or evidence; run only lightweight checks allowed by the assignment.

If validation evidence is incomplete, report that fact directly. Do not spend the review budget trying
to recreate the implementation environment.

## Report

Report findings first with severity, exact file/line evidence, the passing counterexample, and the smallest contract-preserving fix. Then summarize:

- old-contract to new-scenario mapping;
- plan-criterion traceability;
- validation evidence and skipped checks;
- final verdict.

Say `APPROVE` only when no actionable finding remains.
