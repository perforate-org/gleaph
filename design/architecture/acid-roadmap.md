# Gleaph ACID and Consistency Roadmap

Last updated: 2026-06-21 UTC
Status: Phases 0-1 done; Phases 2-6 planned
Anchor timestamp: 2026-06-21 08:26:25 UTC +0000

## Purpose

Turn Gleaph's existing shard-local atomic writes, mutation idempotency, and durable projection
repair into an explicit database consistency contract.

This roadmap does not assume that every GQL transaction must become a cluster-wide ACID
transaction. It separates the guarantees that fit the Internet Computer execution model:

- atomic canonical writes within one canister message;
- durable asynchronous convergence between canisters;
- idempotent roll-forward across graph shards;
- opt-in stronger protocols for invariants that need cross-shard all-or-nothing visibility.

The governing decision is
[ADR 0029](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md).

## Verified baseline

Verified against the repository at `2026-06-21 05:36:08 UTC +0000`.

| ACID property | Implemented scope | Gap |
|---------------|-------------------|-----|
| Atomicity | One synchronous canonical mutation segment on one graph shard | Multi-DML, multi-shard, and graph-to-index work cross commit points |
| Consistency | Graph owns canonical data; durable logs repair label stats and index projections | Public read paths can observe different projection ages |
| Isolation | One canister message handler is serialized and atomic | No transaction-wide snapshot, revision validation, or cross-shard read timestamp |
| Durability | IC replicated state, stable stores, mutation journals, delta log, repair journal | Recovery of federated partial progress depends on retry/admin activity and bounded retention |

The target is not a single binary "ACID compliant" label. Every supported operation must name its
atomicity scope, visibility boundary, retry behavior, and projection freshness contract.

## Non-goals

- Emulating a distributed shared-memory lock manager across canisters.
- Adding cluster-wide MVCC before a query or invariant requires it.
- Treating graph-index or Router aggregate state as canonical graph data.
- Moving Gleaph/IC transaction semantics into `gleaph-gql` or `gleaph-gql-planner`.
- Calling per-plan progress journaling "atomic rollback".
- Preserving development-only persisted layouts when a cleaner invariant owner requires a repack.

## Target consistency model

### Canonical writes

The graph shard owns canonical vertices, edges, properties, and local graph invariants. A supported
local mutation validates and commits all owner-local state in one message segment without an
inter-canister commit point inside the critical section.

### Derived projections

Graph-index postings and Router label statistics are derived projections. Their update contract is:

```text
canonical commit
  -> durable projection intent
  -> at-least-once delivery
  -> idempotent apply
  -> durable contiguous watermark
  -> optional acknowledgement/retention
```

### Federated mutations

Router owns an idempotent roll-forward saga. A mutation is not globally complete until every target
shard has a durable canonical outcome and every projection required by the mutation contract has
reached its watermark.

### Reads

The target read modes are:

- `Eventual`: derived-state latency is preferred over freshness;
- `AtLeast(token)`: read-your-writes barrier for the supplied mutation;
- `Canonical`: owner read where the query shape supports it.

## Sources of truth

| Fact | Owner / source of truth |
|------|-------------------------|
| Vertex, edge, property, label membership | Graph shard canonical stores |
| Shard-local mutation outcome | `GRAPH_MUTATION_JOURNAL` |
| Label-stats projection payload | `LABEL_STATS_DELTA_LOG` |
| Router label counts | Router projection maps, derived through `ROUTER_LABEL_STATS_PROJECTION` |
| Property and label postings | graph-index, derived from graph state |
| Failed posting propagation intent | Graph index repair journal |
| Client request identity and federated progress | Router client mutation journal |

## Phase 0: Contract and terminology

**Status: Done (as of 2026-06-21 08:00:20 UTC +0000).**

Goal: remove ambiguous uses of transaction success before changing persistence or APIs.

Deliverables:

- Implement the accepted ADR 0029 contract through the phases below.
- **Done.** Define `CanonicalCommitted`, `ProjectionPending`, and `Completed` in
  public/internal contracts. Implemented as `MutationLifecyclePhase` in
  `gleaph_graph_kernel::plan_exec`, derived (single source of truth) from the existing
  `RouterMutationRecord` saga fields via `RouterMutationRecord::lifecycle_phase()`.
- **Done.** Clarify that existing graph-journal `Completed` means shard-local replayable
  outcome, not global projection freshness. Documented on
  `gleaph_graph_kernel::plan_exec::MutationJournalState`.
- **Done.** Decide whether public mutation APIs return a richer result or add a
  status/token endpoint. Decision: richer result. The idempotent update entrypoints
  (`gql_execute_idempotent`, `prepared_execute_update_idempotent`) now return
  `GqlQueryResult` with an optional `phase`. The mutation token with per-shard watermarks
  is deferred to Phase 2; only the lifecycle phase ships in Phase 0.
- **Done.** Reconcile ADR 0023's completion invariant with ADR 0024's deferred-index
  success. Both ADRs carry the ADR 0029 vocabulary: a repair-journaled (deferred) flush
  records the shard-local graph-journal `Completed` while the *distributed* mutation stays
  `ProjectionPending` until the index watermark is reached, and Router owns the final
  cross-canister `Completed` transition (ADR 0023 §"Interaction with the mutation journal
  (ADR 0024)" and INV section; ADR 0024 §"Consistency vocabulary (ADR 0029)").
- **Done.** Document the supported consistency mode of every update and index-backed read
  entrypoint. Added the entrypoint consistency-mode table in
  [derived-state-query-semantics.md](../index/derived-state-query-semantics.md).

Tests:

- **Done.** Characterization test for current mutation-journal and projection transitions
  (`lifecycle_phase_tracks_saga_progress`).
- **Done.** A contract test that prevents Router `Completed` while a required
  shard/projection is unfinished (`lifecycle_phase_never_completes_with_outstanding_work`).

Exit criteria:

- **Met.** No active design document equates canonical commit with cross-canister
  convergence. Audited: ADRs 0023/0024/0029, `derived-state-query-semantics.md`
  (principle 5), `stable-memory-inventory.md` (region 39 `GRAPH_MUTATION_JOURNAL`), and the
  ADR 0015 addendum all separate shard-local canonical completion from projection freshness.
- **Met.** `Completed` has one owner and one meaning at each boundary: graph-journal
  `MutationJournalState::Completed` is shard-local replayable; Router
  `MutationLifecyclePhase::Completed` is the cross-canister terminal state.

## Phase 1: Protect the local atomic boundary

**Status: Done (as of 2026-06-21 09:10 UTC +0000).** The boundary is named and structurally
enforced in code, whole-message trap-rollback is proven end to end under PocketIC, and the
canonical-segment mutation canbench baselines (vertex / property / edge) are recorded.

Goal: make the Graph critical section visible in code and tests.

Deliverables:

- **Done (by construction).** Separate remote input acquisition from canonical mutation execution.
  The Router pre-resolves labels, properties, catalog, and seed bindings into `GqlExecutionContext`
  before the graph shard runs; no remote input is fetched mid-mutation.
- **Done.** Ensure the canonical mutation segment contains no inter-canister call/commit point.
  The segment is extracted as the named `apply_canonical_mutation_segment` in
  `crates/graph/src/gql_run.rs`. It takes **no `PropertyIndexLookup` handle** and runs all CALL
  procedures synchronously, so it structurally cannot issue an inter-canister call. The missing
  index parameter is the enforcement; index posting *delivery* is the separate `flush_pending`
  boundary that runs only after the segment.
- **Done.** Commit canonical data, mutation outcome/progress, and required projection intent
  together. `apply_canonical_mutation_segment` performs the store mutation, the durable
  label-stats delta append (projection intent), and the `Incomplete` mutation-journal record with
  no inter-canister `await` between them, so IC semantics commit them in one message segment.
- **N/A for the shard (deferred to Phase 4 saga).** Owner-controlled revision revalidation on a
  read-before-`await`/write-after-`await` path. The graph DML segment has no inter-canister
  `await` between a read and a dependent write; the only read-then-remote-write spans live in the
  Router saga and are addressed in Phases 2 and 4.
- **Done.** Keep GraphStore domain commits as the only write path for affected invariants. Canonical
  vertex/edge/property/label writes flow exclusively through `GraphStore`; the delta log and
  mutation journal are GraphStore-owned commits.

The boundary is shard-local by definition and survives future graph-shard splitting and shard-to-shard
`await`; cross-shard atomicity is composed from shard-local atomic segments above the critical
section, never by extending it across an `await`. See
[ADR 0029 §8](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md). The current
structural enforcement (segment takes no index handle) must generalize to a path-independent guard
once a peer-shard client exists.

Tests:

- **Done.** Reopen-equivalent / commit-together proof: `canonical_segment_commits_canonical_data_and_
  projection_intent_together` (graph `gql_run` tests) shows canonical data, journal progress, and
  projection intent are durable together while index *delivery* is deferred to the repair journal.
- **Done.** Interleaving / stale-precondition rejection: `wire_update_persists_label_stats_delta_and_
  dedupes_retry` and `deferred_index_flush_completes_single_dml_mutation_journal` prove an already
  applied `mutation_id` returns the cached outcome instead of silently re-applying; a partial
  multi-DML bundle stays `Incomplete` (`deferred_index_flush_leaves_multi_dml_mutation_incomplete`).
- **Done.** Trap injection proving whole-message rollback end to end:
  `canonical_segment_trap_rolls_back_whole_message` (PocketIC,
  `adr0029_canonical_segment_rollback`) runs a single linear DML
  (`MATCH (h) INSERT (:RollbackOrphan) DELETE h`) whose plan writes the orphan and then traps at the
  `DELETE` op (the matched hub still has an incident edge). After the trap the orphan does not exist
  and the matched hub and its sink survive — the entire message rolled back. A committing `INSERT`
  control plus the router's "DML atomic section" trap marker rule out a vacuous parse/plan
  rejection. Host unit tests cannot prove this because the in-memory store does not roll back on
  panic.

Benchmarks:

- **Done.** Graph mutation canbench for the canonical segment (wire path, `index = None`):
  `bench_graph_canonical_segment_insert_vertex` (572.41 K instructions),
  `bench_graph_canonical_segment_insert_vertex_with_property` (598.66 K), and
  `bench_graph_canonical_segment_insert_edge` (792.35 K), persisted in `crates/graph/canbench_results.yml`.
  The Phase 1 change is a behavior-preserving extraction (same store/journal/delta calls in the same
  order), so these numbers are the baseline future boundary changes are measured against; no
  regression is expected or observed.

Exit criteria:

- **Met.** Every supported shard-local DML either commits all owner-local state or commits none:
  the canonical segment has no intermediate inter-canister `await`, so it is one atomic message
  segment.
- **Met.** No remote call occurs inside the named canonical critical section: enforced structurally
  by `apply_canonical_mutation_segment` taking no index handle and running CALL procedures
  synchronously.

## Phase 2: Mutation-linked projection watermarks

**Status: Planned.**

Goal: make derived-state freshness observable and usable as a read barrier.

Deliverables:

- Associate failed index repair batches with the originating `mutation_id` and an ordered target.
- Expose whether graph-index work required by a mutation is pending or applied.
- Preserve the existing label-stats `emitted_delta_last_seq` barrier.
- Introduce a mutation token carrying the per-shard watermarks needed for read-your-writes.
- Do not introduce a global snapshot timestamp in this phase.

Tests:

- Deferred index flush returns a canonical outcome and a pending projection status.
- Repair replay advances the mutation-linked watermark exactly once.
- Duplicate delivery and out-of-order delivery do not advance past a gap.
- Upgrade/reopen preserves pending targets and applied cursors.

Benchmarks:

- Repair journal append/drain canbench.
- Router mutation status/token encoding canbench if added to a hot entrypoint.

Exit criteria:

- Given a mutation token, Router can determine whether all required projections caught up.
- Projection completion is durable and idempotent.

## Phase 3: Read consistency API

**Status: Planned.**

Goal: stop silently presenting a stale projection as read-your-writes.

Deliverables:

- Add `Eventual`, `AtLeast(token)`, and supported `Canonical` read modes at the Gleaph integration
  boundary, not in generic GQL crates.
- Return a retryable projection-lag result when a watermark has not been reached.
- Apply barriers to Router label-count fast paths and graph-index seed/membership/property paths.
- Document which query shapes cannot use `Canonical` mode without an owner-side scan.

Tests:

- A successful mutation followed by `AtLeast(token)` cannot observe the pre-mutation projection.
- `Eventual` remains non-blocking and may observe the documented lag.
- Label-count and posting-backed paths enforce their own watermarks independently.
- Federated reads never claim one shared snapshot unless a later protocol provides it.

Benchmarks:

- Router fast-path overhead with and without a token.
- Index lookup overhead for an already-satisfied watermark.

Exit criteria:

- Read freshness is selected or explicitly defaulted, never inferred from mutation success.

## Phase 4: Autonomous federated saga recovery

**Status: Planned.**

Goal: make convergence independent of the original client retrying.

Deliverables:

- Persist an immutable dispatch envelope before the first graph-shard call.
- Add a bounded Router timer/admin recovery step for unfinished shard work.
- Reconcile graph journal outcomes and projection watermarks into monotonic Router progress.
- Exclude unfinished mutations from completed-record TTL eviction.
- Add operator inspection for stuck phase, last error, target shard, and next retry action.
- Bound work per timer message and avoid unsafe lease expiry.

Tests:

- Drop the original client after one shard commits; recovery completes remaining shards.
- Router upgrade during each phase resumes without duplicate graph writes.
- Repeated rejection keeps durable progress and does not spin unboundedly.
- Completed records still compact and expire under ADR 0025.

Benchmarks:

- Router recovery-step canbench at multiple shard counts.
- Client mutation record size and compaction cost.

Exit criteria:

- Every non-terminal federated mutation has an automatic or explicit bounded next action.
- Recovery does not require the original request payload from the client.

## Phase 5: Multi-DML contract

**Status: Planned.**

Goal: remove the existing partial multi-DML ambiguity.

Immediate decision:

- Reject federated multi-DML update bundles until one of the supported contracts below is
  implemented.

Candidate future contracts:

1. **One-shard atomic bundle:** execute all canonical DML in one message segment, provide an
   owner-local overlay for read-your-own-writes, then publish projection intent.
2. **Roll-forward bundle:** persist a per-plan cursor and advertise saga semantics explicitly.
3. **Staged distributed commit:** reserve for a named cross-shard all-or-nothing requirement.

Tests:

- Router rejects unsupported multi-DML before any shard dispatch.
- A supported one-shard bundle traps back to its pre-message canonical state.
- If roll-forward support is selected, retries resume at the persisted plan boundary without being
  described as rollback atomicity.

Benchmarks:

- Bundle validation and one-shard execution at representative statement counts.

Exit criteria:

- No accepted program has unspecified partial-application semantics.

## Phase 6: Selective strong cross-shard invariants

**Status: Deferred pending a concrete product invariant.**

Goal: add TCC, optimistic validation, or staged commit only where eventual roll-forward is
insufficient.

Candidate triggers:

- a cross-shard uniqueness constraint;
- quota/capacity reservation;
- atomic schema publication;
- compare-and-set across owners;
- externally required all-or-nothing multi-shard mutation visibility.

Required gate:

- dedicated ADR naming prepare/commit/cancel states, canonical owner, staged storage, read
  visibility, timeout recovery, retention, upgrade behavior, and conflict semantics;
- failure-injection tests for every message boundary;
- canbench evidence for prepare/commit overhead and storage growth.

Cluster-wide MVCC, a timestamp oracle, and general two-phase commit remain out of scope until this
gate is met.

## Validation matrix

| Failure point | Required outcome |
|---------------|------------------|
| Validation before canonical mutation | No canonical or projection state changes |
| Trap inside canonical message segment | Whole segment rolls back |
| Reject after canonical commit | Canonical outcome and projection intent remain recoverable |
| Router trap after shard success | Retry discovers graph journal outcome |
| Duplicate shard dispatch | Cached outcome; no duplicate canonical write |
| Duplicate projection delivery | Idempotent no-op or same final state |
| Gap in ordered projection stream | Cursor stops before the gap |
| Graph/index/Router upgrade | Stable progress resumes from the same monotonic boundary |
| Client abandonment | Router recovery driver continues or exposes a bounded operator action |

## Required design synchronization

Each implementation patch must review and update the affected subset of:

- [ADR 0015](../adr/0015-label-stats-projection-log.md)
- [ADR 0023](../adr/0023-federated-index-consistency-upgrade-compaction.md)
- [ADR 0024](../adr/0024-mutation-journal-completion-vs-index-flush.md)
- [ADR 0025](../adr/0025-client-mutation-journal-retention-sweep.md)
- [ADR 0027](../adr/0027-graph-mutation-journal-retention.md)
- [Derived-state query semantics](../index/derived-state-query-semantics.md)
- [Stable-memory inventory](../storage/stable-memory-inventory.md)
- public Candid and Rust API documentation.

## Completion reporting

For every phase, report:

- affected canonical and derived state owners;
- changed invariant and enforcement point;
- tests added or updated;
- design documents synchronized;
- `cargo fmt`, check, clippy, and test commands;
- relevant canbench commands and regression judgment;
- skipped checks and remaining failure/recovery risks.
