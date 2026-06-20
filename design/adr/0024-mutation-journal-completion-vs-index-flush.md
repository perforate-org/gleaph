# 0024. Mutation journal completion vs deferred index flush

Date: 2026-06-20
Status: implemented
Last revised: 2026-06-20
Anchor timestamp: 2026-06-20 14:43:28 UTC +0000

## Context

ADR 0015 introduced the graph-local mutation journal (`GRAPH_MUTATION_JOURNAL`) as the
source of truth for graph-shard mutation idempotency. Each `MutationId` entry is
`Incomplete` or `Completed`, and records the emitted label-stats delta sequence range.

The federated DML execution path (`run_wire_plans_inner` in `crates/graph/src/gql_run.rs`)
runs, per mutation plan in program order:

1. apply the store mutation,
2. append the label-stats delta to the durable `LABEL_STATS_DELTA_LOG`,
3. record the journal entry as `Incomplete`,
4. flush the pending property/edge/label index postings to `gleaph-graph-index`
   (`pending::flush_pending`, `edge_pending::flush_pending`, `label_pending::flush_pending`).

The journal is recorded `Completed` only after the **whole** plan bundle finishes
(`run_wire_plans_and_encode`), because the completion `row_count` comes from the last
read plan, which can run after the mutation plans.

ADR 0023 later made index flush failures recoverable: when a posting op fails,
`flush_pending` compensates the partial batch, **persists the whole batch to the durable
repair journal**, and arms the maintenance timer so the index converges to the store
asynchronously. The store mutation and the label-stats deltas are already durable at that
point.

## Problem

A non-trapping post-mutation flush failure permanently wedges a mutation in `Incomplete`.

Concrete sequence (single-statement DML, real index client):

1. Store mutation applies; label-stats delta appended; journal recorded `Incomplete`.
2. `flush_pending` fails, journals the batch for repair, arms the timer — **but still
   returns `Err`** (`crates/graph/src/index/pending.rs`).
3. The `Err` propagates via `?`, skipping `commit_record_completed_mutation_journal`.
   Returning `Err` (rather than trapping) **commits** IC state, so the store mutation,
   the `Incomplete` journal entry, and the emitted deltas all persist.
4. The router calls `recover_mutation_outcome`, which only resolves `Completed` entries;
   for `Incomplete` it returns `None`, so the router returns the error and **does not
   advance the label-stats projection** (`crates/router/src/gql.rs`).
5. Every retry with the same `MutationId` hits the early guard in `run_wire_plans_inner`
   and returns `"mutation … was already applied locally but did not complete"` — forever.

Consequences:

- The mutation is applied to canonical store state but is permanently reported as failed
  (client/store divergence).
- The emitted label-stats deltas are never projected, so router count-only stats stay
  stale for that shard and the delta log can never be acknowledged/trimmed past them
  (unbounded growth).
- The `MutationId` key is stuck `Incomplete` forever.

Facts (verified in code, not assumptions):

- `pending` / `edge_pending` / `label_pending` `flush_pending` all journal-for-repair and
  arm the timer in every failure path, then return `Err`.
- Post-mutation read plans in a bundle can be **index-served** (`skip_index` resets to
  `false` after each mutation plan), so continuing reads after a deferred,
  pre-batch-state flush could observe stale index results.
- A single `MutationId` covers the **entire** plan bundle, which may contain **multiple**
  DML plans.

## Existing Architecture Assessment

The two recovery systems are mutually inconsistent. ADR 0023's repair journal already
owns eventual index convergence; the store mutation and deltas are durable. Therefore a
flush that is durably journaled for repair is **not** a failure of the mutation — only of
the *synchronous* index update. ADR 0015's `Incomplete` state predates the repair journal
and conflates "synchronous flush did not succeed" with "mutation did not apply". No new
subsystem is needed: the fix is to stop coupling mutation-journal completion to the
synchronous flush return value, since flush durability is already owned elsewhere.

## Alternatives

| # | Approach | Benefits | Drawbacks |
|---|----------|----------|-----------|
| A | Router honors "advance projection" for `Incomplete` entries with a known delta range | Smallest change; clears stale stats + unbounded log + permanent retry error | Symptom-level: journal stays `Incomplete`, `row_count` unrecorded; leaves graph/router contract muddy |
| B | Graph: a repair-journaled flush is non-fatal for completion | Fixes root cause; makes ADR 0015 and ADR 0023 consistent in one owner (graph); idempotent retry returns cached success | Larger than A; trailing read-only plans skipped on the deferred path; residual `Incomplete` for a mid-bundle flush failure in a *multi-DML* program |
| C | Idempotent finalize-on-retry: re-run only read-only plans on retry, skip applied mutations, recompute `row_count`, mark `Completed` | Most faithful exactly-once; preserves trailing reads | Highest complexity/risk; journal lacks per-plan progress, so distinguishing applied vs unapplied DML in a multi-DML bundle risks double-apply (new corruption surface); retry coupled to repair-timer schedule |

## Decision

Adopt **B**: decouple mutation-journal completion from the synchronous index flush,
because flush durability is owned by ADR 0023's repair journal.

### Flush signal

`pending` / `edge_pending` / `label_pending` `flush_pending` return a new
`PlanQueryError::IndexFlushDeferred { op, detail }` in the journaled-for-repair failure
paths (both "compensated to pre-batch state" and "compensation failed, batch journaled").
Hard, non-recoverable failures (no index client, no federation routing, sequence
exhaustion) keep their existing error variants. `IndexFlushDeferred`'s `Display` includes
`detail`, so existing operator-facing messages are preserved.

For callers that are not idempotency-journal aware (`run_transaction_block` escape hatch,
admin/maintenance flush handlers, the maintenance timer), `IndexFlushDeferred` behaves as
a normal (recoverable) error or is ignored, exactly as before — they halt safely and the
timer re-applies the journaled batch.

### Completion under deferred flush (`run_wire_plans_inner`)

When a post-mutation flush returns `IndexFlushDeferred` (and no hard error occurred):

- The store mutation(s) and label-stats deltas applied so far are durable, and the index
  will converge via the maintenance timer.
- If there is **no unexecuted DML plan** after the current plan, record the journal entry
  `Completed` (with the mutation `row_count`, emitted delta seq range, and hot forward
  vertices accumulated so far) and return the accumulated result. Trailing read-only plans
  are intentionally skipped, because their index-served reads could observe the
  pre-repair index state. The client call succeeds, the router advances the projection,
  and the index converges asynchronously.
- If there **is** an unexecuted DML plan after the current plan, the bundle cannot be
  safely completed (skipping later DML would silently drop writes on idempotent replay;
  continuing would run later plans against a stale index). Preserve prior behavior:
  propagate the error and leave the entry `Incomplete`.

### Invariant

| Invariant | Owner | Enforcement point |
|-----------|-------|-------------------|
| A mutation whose writes and deltas are durable, with no unexecuted DML remaining, is recorded `Completed` even if the synchronous index flush was deferred to the repair journal | Graph | `run_wire_plans_inner` flush handling |
| Index convergence after a deferred flush | Graph (repair journal) | `flush_pending` journal append + maintenance timer (ADR 0023) |
| A bundle with unexecuted DML after a deferred flush is never recorded `Completed` | Graph | `run_wire_plans_inner` remaining-DML check |

## Consequences

### Positive

- The single-statement DML wedge (the dominant case) is eliminated: a deferred flush
  yields a `Completed` journal, a successful client outcome, and an advanced projection.
- ADR 0015 (mutation idempotency) and ADR 0023 (index repair journal) become consistent:
  both treat a journaled flush as "writes durable, index converges", owned by graph.
- No new subsystem, storage region, or cross-canister protocol; the change lives entirely
  inside the graph execution path and an error variant.

### Trade-offs

- On the deferred path, trailing read-only plans in a multi-statement program are skipped;
  the completed `row_count` reflects the mutation, not a later read. A retry returns the
  cached `Completed` outcome. This is a degraded (not incorrect) result on a rare failure
  path.
- A mid-bundle flush failure in a **multi-DML** program still leaves the entry
  `Incomplete` and errors. This is the safe choice (no silent write loss) and is rarer
  than single-statement DML. It is a pre-existing non-atomicity property of multi-DML
  bundles across the index-flush boundary, not introduced here. Option C (per-plan
  progress) would be required to also recover that case and is deferred.

## Migration

No stable-layout or wire-type change. `GraphMutationJournalEntry` and the delta log are
unchanged. The new `PlanQueryError::IndexFlushDeferred` variant is internal to
`gleaph-graph`. No canister reset required.

## Design Documentation Impact

| Document | Update |
|----------|--------|
| `design/adr/0015-label-stats-projection-log.md` | Note that completion is no longer coupled to synchronous flush success; cross-reference ADR 0024 |
| `design/adr/0023-federated-index-consistency-upgrade-compaction.md` | Note that a repair-journaled flush is non-fatal for mutation-journal completion |
| `design/storage/stable-memory-inventory.md` | Clarify mutation-journal completion semantics under deferred flush |
