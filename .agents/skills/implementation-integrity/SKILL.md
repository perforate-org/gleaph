---
name: implementation-integrity
description: Prevent correctness, boundary, persistence, atomicity, and test-contract defects while implementing Gleaph plans and review fixes. Use for architecture-sensitive code changes, new enum variants or schema forms, storage and index updates, Router/Graph/GQL execution changes, public APIs, parsers, major refactors, or any implementation expected to pass independent review with minimal findings.
---

# Implementation Integrity

Implement from invariants outward. Do not wait for review to discover missing owners, asymmetric
guards, partial writes, duplicated canonical data, or tests that exercise the wrong path.

## 1. Build the contract map before editing

Read the plan, `AGENTS.md`, relevant design contracts, nearby tests, and owning modules. Write down:

- canonical state and its owner;
- derived state and its derivation path;
- write boundary and every precondition it must enforce;
- read/execution paths that rely on the invariant;
- conflict operations that can occur in either order;
- explicitly deferred behavior that must remain fail-closed.

If ownership or semantics are unresolved, stop and report the decision instead of inventing a broad
abstraction.

## 2. Search the complete change surface

Before adding a variant, schema kind, state, or capability, search every old-variant pattern,
accessor, guard, match arm, serializer, wire projection, planner-stat path, index path, benchmark,
test, and active design statement. Classify every hit as:

- must share the new behavior;
- intentionally variant-specific, with a concrete reason;
- obsolete and removable.

Prefer one semantic helper such as `is_named_inline()` over scattered `A || B` knowledge. Exhaustive
matches are better than wildcard arms when a new variant must force a decision.

## 3. Protect canonical state and write atomicity

- Store only canonical facts. Derive offsets, widths, summaries, profiles, and caches from the SSOT
  unless persistence is required and one consistency mechanism owns updates.
- Validate again at the module that owns the write. Do not trust a public/intermediate value merely
  because the normal caller constructed it safely.
- Complete all fallible validation, capacity checks, encoding-size checks, and conflict checks before
  the first canonical or catalog mutation.
- Treat both operation orders as separate contracts: `A then B` and `B then A` must either converge
  safely or reject without partial state.
- Use checked arithmetic and explicit limits. Do not silently saturate, truncate, default, or fall
  back when the contract is fail-closed.
- When backward compatibility is explicitly unnecessary, bump formats cleanly and reject old bytes;
  do not add speculative compatibility shims.

## 4. Preserve boundaries

- Keep Gleaph-specific syntax and execution rules out of `gleaph-gql` and `gleaph-gql-planner`.
- Router owns orchestration, catalogs, names, and global schema; Graph owns graph storage/execution;
  indexes own their lookup state.
- Project only the minimum physical or resolved data needed across a wire. Do not create a second
  logical schema owner in a consumer.
- Deferred functionality must produce a deliberate error before side effects or fallback, not an
  accidental success through an older path.

Use `architecture-integrity`, `gleaph-architecture`, `design-sync`, and `test-contract` for their
specialized rules; this skill coordinates those rules during implementation.

## 5. Make tests prove the advertised path

For each completion criterion, construct one plausible wrong implementation and ensure a test fails:

- remove the new guard;
- return the right error after mutating state;
- call a sibling operation instead of the operation named by the test;
- ignore ordering, one variant/source/direction, or a boundary value;
- accept an earlier error that masks the intended branch;
- make a store setter a no-op while benchmark/test setup still passes.

Tests named for an exact failure mode must invoke that path and assert the exact observable error or
postcondition. Avoid disjunctive assertions that allow the wrong guard to satisfy the test. Test both
orders for symmetric conflicts. Keep combinatorial cases at unit level and one real boundary path in
PocketIC where needed.

## 6. Self-review before handing off

Review the actual diff as if it came from another agent:

1. Re-read the plan and map every TODO/completion criterion to code and a test.
2. Search old variant names and old contract wording again; new edits often create missed call sites.
3. Inspect every error return after the first mutation and every persisted derived field.
4. Check public comments, active design docs, stable-memory inventory, and UTC anchors.
5. Check benchmarks with `benchmark` and validation cost with `cost-aware-validation`: assertions and
   setup stay outside measured closures; persisted artifacts are complete and unrelated noise is
   reverted.
6. Run `cargo fmt --all -- --check`, `git diff --check`, the narrowest owning tests, and scoped
   clippy. Do not launch broad or long suites for reassurance.
7. Inspect `git status --short` and the full diff for unrelated files, ignored plan status, unfinished
   processes, and inaccurate validation claims.

Do not mark a TODO complete from `--no-run`, a background process, or an interrupted runtime. Report
completed, failed, incomplete, and deferred checks separately.

## Handoff gate

Before sending work to review, report:

- invariant/owner changes;
- symmetric call sites audited;
- tests and wrong implementations they detect;
- design and persistence contracts updated;
- exact completed validation;
- skipped checks and remaining risks;
- confirmation that no commit was made when the primary owns commits.

If a known P1/P2 defect remains, keep implementing rather than presenting the slice as review-ready.
