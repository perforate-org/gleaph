# 0091. Path-independent guard for the canonical mutation segment

Date: 2026-08-29
Status: accepted
Last revised: 2026-08-29
Anchor timestamp: 2026-08-29 19:00:00 UTC +0000
Amends: [ADR 0029 §8](0029-shard-local-atomicity-and-cross-canister-consistency.md)

## Context

[ADR 0029 §1](0029-shard-local-atomicity-and-cross-canister-consistency.md) defines the canonical
mutation segment as the supported atomic write boundary:

> One canonical mutation segment executed by one graph shard without an inter-canister commit
> point inside that segment.

The implementation in
[`crates/graph/src/gql_run.rs::apply_canonical_mutation_segment`](../../crates/graph/src/gql_run.rs)
currently relies on two structural properties to keep the segment free of inter-canister calls:

1. the segment takes **no `PropertyIndexLookup` handle** (so the graph-index client cannot be
   reached from inside the segment), and
2. the only `CALL` procedures it executes are the synchronous Gleaph finalize helpers in
   [`crates/graph/src/plan/mutation/gleaph_finalize.rs`](../../crates/graph/src/plan/mutation/gleaph_finalize.rs),
   which never issue inter-canister calls themselves.

ADR 0029 §8 (the "Enforcement note") observes that this is **structural but not
path-independent**:

> When a peer-shard client is introduced, that narrow construction is no longer sufficient on
> its own. Enforcement must then generalize to a path-independent guard — assert "no canonical
> segment is active" at every inter-canister chokepoint (graph-index client, Router call, and
> any peer-shard client) — so a new call path added inside the segment fails loudly instead of
> silently extending the critical section across a commit point.

The gap is therefore an explicit, named defect in the current design. It does not affect
today's behavior (no second inter-canister chokepoint exists), but it becomes load-bearing the
moment a second chokepoint is added — for example, peer-shard client introduction or
Phase-4/6 cross-shard coordination work.

This ADR adopts the path-independent guard in advance of the trigger so the enforcement does
not depend on a future code reviewer's attention.

## Decision

### 1. `CanonicalSegmentGuard` RAII object

Adopt a thread-local depth counter wrapped in an RAII guard as the path-independent guard.

- New module
  [`crates/graph/src/facade/canonical_segment.rs`](../../crates/graph/src/facade/canonical_segment.rs)
  exposes:
  - `pub struct CanonicalSegmentGuard { _private: () }`,
  - `pub fn CanonicalSegmentGuard::enter() -> Self` (increments the depth counter),
  - `Drop` for `CanonicalSegmentGuard` (decrements the depth counter; traps if the counter
    would underflow),
  - `pub fn canonical_segment_depth() -> u32` (read-only accessor for tests and assertions),
  - `pub fn assert_no_canonical_segment(chokepoint: &'static str)` (chokepoint-side trap
    helper).

- The depth counter is a `thread_local! { static CANONICAL_SEGMENT_DEPTH: Cell<u32> }`.
  ICP canister execution is single-threaded (Property 1 in
  [the ICP message-execution reference](https://docs.internetcomputer.org/references/message-execution-properties.md)),
  so the thread-local scope is the message-execution scope. The counter resets across message
  executions because the WASM instance's thread-locals are reconstructed at each message
  boundary (Property 2).

- The depth counter is a `u32`, not a `bool`, to permit future legitimate nested read phases
  inside the segment (none today). The contract is "enter balanced by Drop"; a Drop that
  observes a depth that does not return to zero traps the message.

### 2. Apply the guard in the canonical segment

[`crates/graph/src/gql_run.rs::apply_canonical_mutation_segment`](../../crates/graph/src/gql_run.rs)
must enter the guard as its first statement and hold it for the lifetime of the segment:

```rust
async fn apply_canonical_mutation_segment(
    store: &GraphStore,
    mutation_ops: &[gleaph_gql_planner::plan::PlanOp],
    ...
) -> Result<PlanMutationBindings, GqlRunError> {
    // ADR 0091: pin the canonical segment as no-inter-canister-call.
    // Drop balances the depth counter; the trap-on-Drop-mismatch catches any
    // future re-entry that violates the no-`await`-between-writes invariant.
    let _canonical_segment_guard = CanonicalSegmentGuard::enter();
    ...
}
```

The existing "no `PropertyIndexLookup` handle" and "synchronous `CALL` procedures"
guarantees are retained as defense-in-depth. They are no longer the primary guarantee; the
primary guarantee is the guard.

### 3. Assert at every inter-canister chokepoint

Every acquisition boundary that can issue an inter-canister call MUST invoke
`assert_no_canonical_segment("chokepoint_name")`. Today the only such chokepoint in graph
is the `PropertyIndexLookup` acquisition in
[`ExecutorContext::new`](../../crates/graph/src/plan/query/executor/context.rs).

The check is an `assert_eq!(canonical_segment_depth(), 0, ...)`. In a canister build it traps
the whole message (Property 5), rolling back any canonical writes the segment had made
before the violation. In a host build it is a panic; the host test surfaces the violation
before the segment can leak state.

### 4. PR-review checklist for new inter-canister chokepoints

Adding a new inter-canister chokepoint to a graph code path is a checklist item in PR
review. The acquisition boundary must call `assert_no_canonical_segment(...)`. A PR that
introduces a new chokepoint without the assertion must not merge.

This is the only enforcement against future drift, but unlike the original structural
enforcement it does not depend on the second chokepoint happening to "look like" the first.

### 5. Read paths are unaffected

Read paths (`execute_plan_query_bindings` and friends) run **outside** the canonical segment.
They acquire `PropertyIndexLookup` freely; `assert_no_canonical_segment` passes for them
because the depth counter is zero.

## Trigger to introduce

This Amendment is introduced **voluntarily**, in advance of the trigger named in ADR 0029
§8. The voluntary introduction is justified by:

1. The implementation cost is small (one new module ~70 lines + 1-line change to
   `apply_canonical_mutation_segment` + one assertion line at each existing chokepoint).
2. The defect is named in ADR 0029 §8 as "expected when a second inter-canister path first
   appears ... not before"; voluntary adoption moves the burden from a future PR's reviewer
   to a deliberate, reviewable change.
3. The guard is exercised by both host unit tests (depth-balance, outside-guard pass,
   inside-guard trap, nested-enter depth) and PocketIC E2E (whole-message rollback on
   trap), so it is safe to land before any second chokepoint arrives.

The original §8 trigger ("when a second inter-canister path first appears") still applies
for future chokepoints: each new chokepoint PR must add its own `assert_no_canonical_segment`
call per Decision 4.

## Consequences

### Positive

- The canonical segment is now defensible from any new inter-canister chokepoint by a
  single line of defense at the chokepoint boundary.
- ADR 0029 §1's "no remote call inside the named canonical critical section" invariant
  upgrades from "structural and narrow" to "path-independent and reviewed".
- Read paths remain unchanged: they were already outside the segment, and the guard is
  enter-only at the segment boundary.
- The Drop-balance trap catches re-entry violations at runtime, not in code review.

### Trade-offs

- A small amount of thread-local cell traffic per canonical segment (one increment and one
  decrement per segment, plus chokepoint reads on the read path; the read path is not
  affected because `canonical_segment_depth() == 0` is the common case).
- A new piece of API surface (`CanonicalSegmentGuard` and the chokepoint helper) that
  future code authors must be aware of. Mitigation: ADR 0029 §8's checklist + this ADR's
  PR-review rule.

### Neutral

- The "no `PropertyIndexLookup` handle" / "synchronous `CALL` procedures" guarantees remain
  in place. They are now redundant with the guard but documented as defense-in-depth; this
  ADR does not remove them.

## Invariants

| Invariant | Owner | Enforcement point |
|-----------|-------|-------------------|
| The canonical mutation segment carries no inter-canister call/commit point | Graph | `CanonicalSegmentGuard::enter()` at segment start; `assert_no_canonical_segment(...)` at every inter-canister chokepoint |
| A new inter-canister chokepoint added to graph code paths calls `assert_no_canonical_segment(...)` at its acquisition boundary | Graph (PR-review checklist) | Code review; documented as ADR 0091 Decision 4 |

## Implementation status

**Pending.** This ADR and the design-SSOT updates land as documentation; the implementation
patch (new module, one-line segment enter, chokepoint assertions, host + PocketIC tests) is a
follow-up implementation slice. The follow-up slice must land before any second inter-canister
chokepoint is added to a graph code path, so ADR 0091 Decision 4 (PR-review checklist) is
the active guard until the implementation slice lands.

Components planned for the implementation slice:

- `crates/graph/src/facade/canonical_segment.rs` (new module, ~70 lines + 4 unit tests)
- `crates/graph/src/gql_run.rs::apply_canonical_mutation_segment` (one-line `enter()` at the
  segment start)
- `crates/graph/src/plan/query/executor/context.rs::ExecutorContext::new` (one-line
  `assert_no_canonical_segment("executor_context_new")`)
- `crates/pocket-ic-tests/tests/adr0091_path_independent_guard.rs` (new PocketIC test file
  with three scenarios: segment with no inter-canister call succeeds; read path outside
  segment can call index; inter-canister call inside segment traps the whole message)

Validation to run on the implementation slice:

- `cargo fmt --all -- --check`,
- `cargo clippy -p gleaph-graph --all-targets --all-features -- -D warnings`,
- `cargo test -p gleaph-graph --lib canonical_segment`,
- `cargo test -p gleaph-pocket-ic-tests --test adr0091_path_independent_guard`.

## Related decisions

- [ADR 0029 §1](0029-shard-local-atomicity-and-cross-canister-consistency.md) — canonical
  mutation segment atomicity boundary
- [ADR 0029 §8](0029-shard-local-atomicity-and-cross-canister-consistency.md) — original
  "Enforcement note"; this ADR formalizes the path-independent guard that §8 reserved for the
  moment a second inter-canister path appears
- [ACID and consistency roadmap](../architecture/acid-roadmap.md) — Phase 1 exit criteria
  updated to reflect the path-independent guarantee
- [`gleaph-mvcc-and-ic-atomicity.md`](../research/gleaph-mvcc-and-ic-atomicity.md) —
  research note identifying the gap this ADR closes
- [`gleaph-mvcc-design-review.md`](../research/gleaph-mvcc-design-review.md) — review note
  that ranked the gap as P1
- [`gleaph-segment-guard-implementation.md`](../research/gleaph-segment-guard-implementation.md) —
  implementation research note that this ADR formalizes