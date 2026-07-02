---
name: adversarial-test-review
description: Review test additions, consolidations, and refactors against their plan and behavioral contracts by constructing wrong implementations that could still pass. Use for PocketIC/E2E consolidation, lifecycle tests, fail-closed error paths, filtering/ranking/path selection, multi-source or multi-direction coverage, and any review where assertion strength or fixture ordering determines correctness.
---

# Adversarial Test Review

Review the actual diff independently. Do not approve from the implementation report alone.

## Inputs

Read, in this order:

1. The plan and its completion criteria.
2. The pre-change tests from `HEAD` or the review base.
3. The current diff and resulting test file.
4. The implementation report and validation evidence.

Keep read-only reviews read-only. Do not rerun long tests, clippy, builds, or benchmarks unless explicitly assigned.

## Contract Traceability

Map every removed or changed contract to a current named scenario and assertion. For each plan criterion record:

- fixture/setup that establishes the precondition;
- operation under test;
- exact observable result;
- postcondition that detects forbidden state changes;
- assertion that would fail if the criterion were broken.

Missing traceability for a required criterion is a finding.

## Mandatory Counterexample Pass

For every scenario, write at least one plausible wrong implementation that would still satisfy its assertions. Check these failure patterns explicitly:

- returns the expected error after mutating canonical or derived state;
- selects the wrong single member while preserving the expected row count;
- ignores a predicate, cost, ordering key, range arm, or filter source;
- exercises only one of several properties, sources, arms, labels, shards, or traversal directions;
- lets earlier fixture state mask a later scenario;
- uses identical/idempotent follow-up input that hides an unauthorized or partial write;
- deduplicates or merges inputs so an advertised independent arm is never observed;
- reports success based only on compilation, `--no-run`, or an unfinished background process.

Strengthen observability with exact identities, values, ordering, path shape, call counts, or bounded state postconditions as appropriate. Row counts alone are insufficient when the wrong member can produce the same count.

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

## Report

Report findings first with severity, exact file/line evidence, the passing counterexample, and the smallest contract-preserving fix. Then summarize:

- old-contract to new-scenario mapping;
- plan-criterion traceability;
- validation evidence and skipped checks;
- final verdict.

Say `APPROVE` only when no actionable finding remains.
