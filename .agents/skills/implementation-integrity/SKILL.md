---
name: implementation-integrity
description: Prevent correctness, boundary, persistence, atomicity, and test-contract defects while implementing Gleaph plans and review fixes. Use for architecture-sensitive code changes, new enum variants or schema forms, storage and index updates, Router/Graph/GQL execution changes, public APIs, parsers, major refactors, or any implementation expected to pass independent review with minimal findings.
---

# Implementation Integrity

Implement from invariants outward. Do not wait for review to discover missing owners, asymmetric
guards, partial writes, duplicated canonical data, or tests that exercise the wrong path.

## Review-fix operating mode

When addressing review findings, treat the coordinator's fix queue as an executable checklist:

1. Copy every required correction into a short checklist before editing.
2. Map each item to the exact production branch, test assertion, and postcondition it requires.
3. Change only those owners unless the fix exposes a directly necessary prerequisite.
4. After formatting, re-open the exact changed lines and check every item against the final file.
5. Search for broadened assertions, fallback branches, or duplicate guards introduced during the fix.
6. Report an item complete only when the final diff—not memory of the edit—proves it.

Do not replace an exact error requirement with `A | B`, multiple accepted substrings, or a weaker row-count
assertion. Do not report that code or a test was tightened, removed, or centralized without re-reading the
final lines that establish that claim.

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
- For every operation that deletes or overwrites existing state, identify the first destructive statement
  explicitly. Trace every possible error reachable after it. Move schema/category checks, assignment
  classification, encoding, and persistence validation before that statement, or provide an owning atomic
  transaction with a tested rollback contract.
- A rejection test for destructive operations must seed distinct pre-existing state and assert that it
  survives unchanged. Merely asserting the returned error does not prove atomicity.
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

Use `architecture-integrity`, `gleaph-architecture`, `code-quality`, `design-sync`, and
`test-contract` for their specialized rules; this skill coordinates those rules during
implementation.

## 5. Make tests prove the advertised path

For each completion criterion, construct one plausible wrong implementation and ensure a test fails:

- remove the new guard;
- return the right error after mutating state;
- call a sibling operation instead of the operation named by the test;
- ignore ordering, one variant/source/direction, or a boundary value;
- accept an earlier error that masks the intended branch;
- make a store setter a no-op while benchmark/test setup still passes.

For idempotent stores, retry with the same idempotency identity but deliberately different
non-identity payload fields; assert that the original canonical and derived state is returned and
preserved. For prefix scans, do not invent a maximum user-value sentinel unless the domain proves a
true maximum/successor; test the highest valid adversarial key and an adjacent prefix.

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
8. Apply `code-quality`: review new signatures, flags, visibility, nesting, helper count, obsolete
   paths, net code growth, and whether a smaller existing abstraction can express the same contract.
9. For review fixes, re-run the original finding as a counterexample against the final code and inspect each
   required assertion literally. If any checklist item is still absent, do not notify the reviewer.

## 7. Dispose of newly discovered gaps

Implementation work often exposes a defect or missing capability outside the original diff. Do not
leave it only in terminal scrollback, a temporary report, an ignored plan, or the completion summary.

Before handoff, classify every material discovery:

1. **Fix in the current slice** when it is a correctness/security defect, blocks the current
   contract, has a clear owner, and stays reviewable.
2. **Create a prerequisite slice** when it blocks the current work but needs an independent diff,
   validation loop, or commit. Pause rather than weakening the original completion criteria.
3. **Record it in `design/implementation-gaps.md`** when it is real but non-blocking, unresolved, or
   would materially expand scope. Include observed behavior, owner, evidence, impact, next decision,
   and status.
4. **Dismiss it only with evidence** that the behavior matches an active contract.

When a later slice resolves a ledger entry, update the same entry with the fixing commit and owning
regression test. Do not create a second roadmap or duplicate an ADR's normative design in the ledger;
link to the authoritative document instead.

Do not mark a TODO complete from `--no-run`, a background process, or an interrupted runtime. Report
completed, failed, incomplete, and deferred checks separately.


## Implementation rules

### Fail-closed result types

A public ingress handler must not return a terminal result envelope for a
non-terminal canonical state. A terminal callback type such as `ProvisionResult`
is reserved for terminal outcomes; a first admission or an idempotent replay is
non-terminal and must return a distinct typed ingress response (for example,
`ProvisionAcceptResponse` with `Accepted` / `Replay` variants).

Distinguish ingress responses from terminal callbacks. Do not overload a terminal
callback type with non-terminal ingress data, and never synthesize a `Failed`
reason to fit a non-terminal state into a terminal callback.

Add a wrong-impl test for every typed ingress response: assert that a wrong
implementation returning a terminal `Failed` for a successful admission would fail.

### Preflight-then-co-write

A store facade that mutates multiple regions must preflight all lock, index, and
foreign-key conflicts **before** any write, then co-write in a single block. The
preflight must cover every derived row and lock the co-write will touch. If the
preflight fails, the function returns the typed error and the worktree is unchanged.

A `store.remove` call inside an error-recovery branch of a public ingress handler is a
code smell. The canonical pattern is a single facade method that performs preflight
plus co-write atomically (for example, `insert_with_intent_locks`).

Add a wrong-impl test for every multi-region write: seed an existing state with a
canonical record, derived rows, and held locks; attempt a conflicting insert; assert
that the existing canonical record, locks, and exact derived mapping all survive and
that the conflicting request leaves no state.

### Canonical key drives lookup

A handler that resolves a canonical record must use the exact canonical key fields,
not a scan-and-pick. If the canonical key is composite (for example,
`(request_id, deployment_id)`), the wire shape must carry all composite-key fields,
and the store facade must expose `get(key1, key2, ...)` — not a partial-key scan.

A `request_id`-only lookup is forbidden when the canonical key includes more fields,
even if the implementation documents a uniqueness assumption. Uniqueness is not a
substitute for exact-key addressing.

Document any wire-shape change that diverges from a prior slice's shape as a primary-final
boundary exception driven by a durable protocol reason.

### Dormant helper

A helper that returns a typed result must not return a wrong-shape result for
non-applicable inputs. Non-applicable inputs return `Err`. A wrong-shape result for a
non-applicable input is a dormant defect that will reappear when the next caller invokes
the helper. Fix the **helper itself**, not the call site.

Add a wrong-impl test asserting that a wrong implementation returning `Ok(...)` for a
non-applicable input would fail.

### Post-commit error

A handler that performs a durable state mutation (advance + lock release + version
persist) must not return a recoverable `Err` after the mutation. A real count-mismatch
postcondition is corruption, not a recoverable flow. Either remove the post-commit error
contract, or trap/assert and roll back the whole message.

Add a wrong-impl test asserting that a wrong implementation returning a recoverable
`Err` after the durable state mutation would fail. Remove `cfg(test)` seams that
fabricate a post-commit error to make a test pass.

### Comment vs assertion

A comment that says an invariant is asserted must point to the actual assertion call.
Comments are not assertions. A comment that claims an invariant is held but the test body
does not actually check the invariant is a false-positive.

In review, flag every comment-asserted invariant without a corresponding assertion call
as a P2 finding: the test does not test what the comment says it tests.

### Review-incident comment

Product code must not carry review-incident wording. Comments such as
`// REV6 P1-3 boundary exception` or `// primary final finding #N` are review process
artifacts, not durable protocol reasons. Rewrite them to state the durable protocol reason
(for example, `// RouterProvisionAck carries deployment_id so the canonical key can be
formed without ambiguity across deployment bindings.`) or remove them.

Plan files may carry review-process documentation because they are the durable record of
the review process; product code does not.

### Compensating rollback ownership

A rollback or compensating write may undo ONLY effects that are PROVEN to have been created by the
current operation. Lifecycle state alone (e.g., "still in AwaitingAck") is NOT ownership evidence — a
pre-existing record in that state belongs to a prior invocation and must be preserved.

Establish synchronous ownership at the boundary that creates the effect. For example, a
create-or-return API must return an `Inserted` vs `Existing` signal, and the rollback gate must
require `Inserted` (or an equivalent ownership proof) in addition to any lifecycle state check.

Add a wrong-impl test that seeds a pre-existing record in the same lifecycle state, retries the
operation so the create-or-return API returns `Existing`, fails the downstream send, and asserts
that the pre-existing record and its derived state survive unchanged. See also
`adversarial-test-review` for the review-side framing of the same rule.

### Owner-identity locks

A lock, lease, or reservation that gates access to a shared resource must be bound to the identity
of the request that created it. The mere presence of a lock is not enough: a pre-existing lock held
by a different request must be treated as a conflict, not as satisfaction of a preflight.

Store the owner identity in the lock value (for example, the canonical request key plus an
operation fingerprint). Preflight checks must require both presence and owner equality. Release
and compensating rollback must only remove locks owned by the operation performing the release.

Add a wrong-impl test that seeds a lock held by a different owner and asserts that the new
operation is rejected or, for release, that the foreign lock survives.

### Audit-before-return-on-failure

A handler that emits an audit or telemetry row for an authorization decision
must persist the audit row BEFORE returning the error to the caller. An audit
row written after the error return is either never written (early-return path)
or written but not observable to the caller, which breaks the audit trail's
durability contract.

For each error branch (InvalidState, AlreadyExists, UnknownDeployment,
NotAuthorized, Conflict, etc.), call the audit facade (for example
`BootstrapAuthStore::put_record`) to write the corresponding `Reject*` audit
row first, then return the typed error. A preflight that returns Err before any
audit write leaves the audit log silent for that decision, which is
non-recoverable for an after-the-fact investigator.

Add a wrong-impl test asserting that a wrong implementation that returns the
error BEFORE writing the audit row would fail (e.g. by checking the audit log
contains the expected Reject* entry after the error return).

## Handoff gate

Before sending work to review, report:

- invariant/owner changes;
- symmetric call sites audited;
- tests and wrong implementations they detect;
- design and persistence contracts updated;
- exact completed validation;
- skipped checks and remaining risks;
- complexity introduced, signatures with five or more parameters, and obsolete code removed;
- confirmation that no commit was made when the primary owns commits.
- disposition of every material implementation gap discovered during the slice, including the
  ledger id or prerequisite plan when it was not fixed immediately.

If a known P1/P2 defect remains, keep implementing rather than presenting the slice as review-ready.
