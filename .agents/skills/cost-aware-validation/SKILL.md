---
name: cost-aware-validation
description: Design or review test and benchmark fixtures for strong contract coverage at bounded cost. Use when choosing test layers or adding or consolidating expensive fixtures.
---

# Cost-Aware Validation

Optimize validation cost only after preserving the behavioral contract. A faster suite that permits
false passes is a regression.

For command execution, budgets, and completion evidence, follow
[rust-workflow](../rust-workflow/SKILL.md). When assigned independent validation, use its
[non-mutation rules](../rust-workflow/SKILL.md#independent-validation). This skill owns test-layer
and fixture design, not a separate execution workflow.

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
- benchmark fixture shape and measured variable.

For command results, artifact status, and outstanding checks, use
[rust-workflow's completion evidence](../rust-workflow/SKILL.md#completion-evidence).
