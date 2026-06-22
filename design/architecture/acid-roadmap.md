# Gleaph ACID and Consistency Roadmap

Last updated: 2026-06-21 UTC
Status: Phases 0-4 done (Phase 3 `Canonical` deferred; Phase 4 autonomous recovery is projection-only, canonical re-dispatch stays explicit-retry); Phases 5-6 planned
Anchor timestamp: 2026-06-21 11:05:29 UTC +0000

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

**Status: Done (as of 2026-06-21 11:05:29 UTC +0000).** The mutation token is *issued* and
the graph-index watermark is *exposed*; enforcing the `AtLeast(token)` read barrier remains
Phase 3 (issue-only scope).

Goal: make derived-state freshness observable and usable as a read barrier.

Deliverables:

- **Done.** Associate failed index repair batches with the originating `mutation_id`. The
  durable repair journal value type is now
  `gleaph_graph::facade::stable::repair_journal::RepairJournalEntry { mutation_id, op }`
  (stable region 41, backward-incompatible repack — see
  [stable-memory-inventory.md](../storage/stable-memory-inventory.md)). `flush_pending`
  (vertex / edge / label) threads the federated `mutation_id` to the append site;
  `mutation_id == 0` is the reserved *untracked* sentinel (e.g. maintenance-timer flushes).
- **Done.** Expose whether graph-index work required by a mutation is pending or applied.
  Graph query `index_pending_min_mutation_id() -> Option<MutationId>` returns the smallest
  tracked unapplied mutation id (the mutation-linked index watermark); `None` means all
  tracked index work drained. A read for mutation `M` is index-satisfied on the shard iff the
  result is `None` or `M < value`. Derived single-source-of-truth from the repair journal, not
  a separate stored cursor.
- **Done.** Preserve the existing label-stats `emitted_delta_last_seq` barrier. Unchanged; the
  mutation token carries each shard's `emitted_delta_last_seq` as its label-stats watermark.
- **Done.** Introduce a mutation token carrying the per-shard watermarks needed for
  read-your-writes. `gleaph_graph_kernel::plan_exec::MutationToken { mutation_id, shards:
  [{ shard_id, label_stats_seq }] }`, returned on `GqlQueryResult.token` for idempotent DML.
  The index barrier is keyed by the monotonic `mutation_id`; label-stats by each shard's seq.
- **Done (by construction).** No global snapshot timestamp introduced; watermarks are per-shard
  seqs and a monotonic mutation id.

Tests:

- **Done.** Deferred index flush links the repair batch to its `mutation_id` and pins the
  watermark (`deferred_flush_links_repair_batch_to_mutation_id`, graph `index::pending`).
- **Done.** Repair replay advances the mutation-linked watermark exactly once and ignores the
  untracked sentinel (`min_tracked_mutation_id_pins_lowest_unapplied_and_ignores_untracked`,
  graph `index::repair_journal`).
- **Done.** End-to-end token issuance + watermark exposure under PocketIC
  (`router_idempotent_dml_issues_mutation_token_and_exposes_index_watermark`).
- **Done.** Token candid round-trip (`mutation_token_candid_roundtrip`,
  `gql_query_result_carries_phase_and_token`, graph-kernel `plan_exec`).
- **Deferred to Phase 3.** Duplicate/out-of-order delivery gap handling and upgrade/reopen
  watermark persistence are exercised at the barrier-enforcement layer; the underlying repair
  journal already persists across upgrade via stable region 41 (ADR 0023 D5 tests).

Benchmarks:

- **Done (no regression).** The canonical-segment mutation canbenches
  (`bench_graph_canonical_segment_insert_vertex` / `_with_property` / `_insert_edge`) are
  within noise after threading `mutation_id` (the happy-path flush is a no-op under bench init,
  so the repair-append path is not on the measured segment). A dedicated repair-append/drain
  canbench is deferred — it is a cold, deferred-maintenance path, not a hot entrypoint.

Exit criteria:

- **Met.** Given a mutation token, Router can determine whether all required projections caught
  up: label-stats via each shard's `label_stats_seq` against the projection cursor, graph-index
  via `index_pending_min_mutation_id` against the token's `mutation_id`.
- **Met.** Projection completion is durable (stable region 41) and idempotent (drain
  re-application is idempotent; watermark derived from the journal contents).

## Phase 3: Read consistency API

**Status: Done (as of 2026-06-21 11:37:56 UTC +0000), `Canonical` deferred.** `Eventual` and
`AtLeast(token)` are implemented and enforced; `Canonical` is accepted on the wire but rejected at
runtime (deferred — see Deferred below), so callers never silently receive `Eventual` semantics
under a stronger label.

Goal: stop silently presenting a stale projection as read-your-writes.

Deliverables:

- **Done.** Read modes live at the Gleaph integration boundary, not in the generic GQL crates:
  `gleaph_graph_kernel::plan_exec::ReadMode { Eventual, AtLeast(MutationToken), Canonical }`
  (`Eventual` is the `Default`). New router composite-query entrypoints
  `gql_query_with_consistency(query, params, ReadMode)` and
  `prepared_execute_query_with_consistency(name, params, ReadMode)` carry it; the existing
  `gql_query` / `prepared_execute_query` keep `Eventual` semantics (back-compatible).
- **Done.** Unmet watermarks return a retryable `RouterError::ProjectionLag { shard_id, watermark,
  required, current }` **without serving stale state**; the caller retries after the projection
  drains.
- **Done.** The barrier is enforced once in `run_gql` (and the prepared path) before *any* read
  shape is dispatched, so the Router label-count fast path, graph-index seed, and graph-shard scan
  are all gated uniformly (no per-path gap). For `AtLeast(token)`, each token shard must satisfy
  both its label-stats projection cursor (`label_stats_projection_cursor`, resolved locally) and
  its graph-index watermark (`index_pending_min_mutation_id`, queried per shard: index-satisfied
  iff `None` or `mutation_id < value`).

Deferred:

- **`Canonical` mode is deferred.** It is rejected at runtime (`InvalidArgument`) rather than
  silently downgraded. Routing each shape to an owner-side scan (bypassing the label-count fast
  path and index seed) and documenting which shapes cannot use `Canonical` without an owner scan
  is left to a follow-up; the `ReadMode::Canonical` variant is reserved on the wire so adding it
  later is backward-compatible.

Tests:

- **Done.** A successful idempotent DML followed by `AtLeast(token)` is served read-your-writes,
  while a token whose watermark is forced past the projection cursor returns retryable
  `ProjectionLag`; `Canonical` is rejected and `Eventual` is non-blocking
  (`router_atleast_read_barrier_serves_when_satisfied_and_lags_when_unmet`, PocketIC).
- **Done.** Barrier decision logic host unit tests: `Eventual` no-op, `Canonical` rejected,
  label-stats lag short-circuit returns `ProjectionLag`, empty token satisfied
  (`gql::tests::read_barrier_*`).
- **Done.** `ReadMode` candid round-trip across all variants (`read_mode_candid_roundtrip_all_variants`,
  graph-kernel `plan_exec`).

Benchmarks:

- **Deferred (no hot-path change).** The `Eventual` path is a single match arm with no added work;
  the `AtLeast` barrier adds one inter-canister query per token shard, which is a deliberate
  per-read cost, not a regression to an existing entrypoint. No canbench added for the barrier
  (it is a control-plane read gate, not a storage hot path).

Exit criteria:

- **Met.** Read freshness is selected (`ReadMode`) or explicitly defaulted (`Eventual`), never
  inferred from mutation success. A read that cannot meet its requested barrier fails retryably
  instead of serving a stale projection.

## Phase 4: Autonomous federated saga recovery

**Status: Core done (autonomous recovery is projection-only; canonical re-dispatch stays
explicit-retry, see below).**

Goal: make convergence independent of the original client retrying.

Scope decision (projection-only autonomous recovery): the background timer drives only the safe,
idempotent half of recovery — projection/index convergence for sagas whose canonical writes are
already durable. It deliberately does **not** re-dispatch canonical DML, because autonomous shard
re-execution is the single operation that risks double-apply. Unfinished canonical writes
(`CanonicalPending`) are resumed by explicit idempotent retry, surfaced via `mutation_status`. Full
autonomous canonical re-dispatch can be added later without reworking the timer/lease/status
machinery (it would add envelope `plan_blob`/`params` persistence plus a re-dispatch branch).

Deliverables:

- Persist an immutable dispatch envelope before the first graph-shard call. **(Pre-existing; Phase 4
  relies on it.)**
- Bounded, self-rescheduling Router recovery timer (`ic-cdk-timers`) for unfinished **projection**
  work; armed after each idempotent DML and re-armed from `init` / `post_upgrade`. **Done**
  (`crates/router/src/recovery.rs`).
- Reconcile graph journal outcomes and projection watermarks into monotonic Router progress. **Done**
  (`gql::recover_mutation_record`, idempotent + cursor-guarded).
- Exclude non-terminal mutations from TTL eviction (terminal-only predicate). **Done** (ADR 0025
  revision; `evict_expired_client_mutation_keys`).
- Operator/SDK inspection for stuck phase, last error, target shard, next retry action. **Done**
  (`mutation_status` router query).
- Bound work per timer message and avoid unsafe lease expiry. **Done** (per-tick scan budget;
  `routing_lease_ns` / `ROUTING_LEASE_TTL_NS` reclaim is safe only pre-envelope).

Tests:

- Drop the original client after one shard commits; recovery completes remaining shards. **Done** —
  host tests cover the recovery decision logic (scan selection, projection convergence record
  mutations, lease reclaim, TTL retention, status derivation); a PocketIC test asserts the timer is
  armed, runs, and is a safe no-op on a terminal saga, plus `mutation_status` wiring. A full
  end-to-end convergence test crashes one shard mid-saga, asserts the saga persists as
  `CanonicalPending`, that the autonomous timer leaves it pending without double-applying while the
  shard is down, and that restarting the shard plus an idempotent retry converges it to `Completed`
  with both shards updated (`router_recovers_non_terminal_federated_saga_via_idempotent_retry`).
  Note: the recovery driver is projection-only by design, so the timer converges a `ProjectionPending`
  saga autonomously, whereas a `CanonicalPending` saga (an outstanding canonical shard write) is
  resumed by the idempotent retry path, never by re-dispatching canonical DML from the timer.
- Autonomous `ProjectionPending` -> `Completed` convergence with no client in the loop. **Done** —
  that persisted state (canonical durable on all shards, projection advanced on some) is unreachable
  through the black-box DML path, which advances every shard's projection inline before returning, so
  a test-only seam (gated by the `pocket-ic-e2e` feature: `test_inject_projection_pending_saga`)
  injects the state referencing a real prior mutation, and the test asserts a single recovery-timer
  run advances the lagging shard's projection and finalizes the record without any idempotent retry
  (`router_recovery_timer_converges_projection_pending_saga_autonomously`).
- Router upgrade during each phase resumes without duplicate graph writes — recovery is projection-
  only and idempotent; `post_upgrade` re-arms the timer.
- Repeated rejection keeps durable progress and does not spin unboundedly — timer backs off to a
  relaxed delay and stops a lap that finds no recoverable saga.
- Completed records still compact and expire under ADR 0025 — unchanged; terminal records evict as
  before.

Benchmarks:

- Router recovery-step canbench at multiple shard counts.
- Client mutation record size and compaction cost.

Exit criteria:

- Every non-terminal federated mutation has an automatic or explicit bounded next action.
- Recovery does not require the original request payload from the client.

## Phase 5: Multi-DML contract

**Status: Rejection gate done. Contract 1 (one-shard atomic) implemented for completely-new
INSERT-only bundles and the anchored single-shard subset. Contract 2 (roll-forward bundle)
implemented for single-anchor threaded bundles that fan out to multiple shards. Staged distributed
commit (contract 3) still reserved.**

Goal: remove the existing partial multi-DML ambiguity.

Immediate decision (implemented):

- Reject federated multi-DML update bundles before any shard dispatch, until one of the supported
  contracts below is implemented. A bundle is rejected iff it is a write program whose statement
  block contains **more than one top-level DML statement** (`StatementBlock::first` plus each
  `NEXT` statement, counted as one regardless of how many DML parts a single statement holds)
  **and** the target graph is **federated** (more than one live shard). The Router returns
  `RouterError::UnsupportedMultiDmlBundle` before resolving seeds or dispatching to any shard, so
  no canonical or projection state changes. Single-shard multi-DML stays shard-local atomic
  (Phase 1); a single federated DML statement converges via the Phase 4 roll-forward saga;
  completely-new INSERT-only bundles are accepted under contract 1 below. The gate is enforced at
  both ingress points that own the AST: ad-hoc `gql_query*`/`gql_update*` (`run_gql`) and
  prepared-plan registration (`prepared_register`). A prepared plan registered against a
  single-shard graph that is later re-sharded is an orthogonal prepared-plan staleness concern,
  not covered by this gate.

Candidate future contracts:

1. **One-shard atomic bundle** *(implemented — completely-new INSERT-only and anchored single-shard
   subsets):* execute all canonical DML in one message segment on a single shard, with the shard's
   existing single canonical segment providing read-your-own-writes between statements, then publish
   projection intent. Two qualifying subsets are admitted, both of which provably touch one shard:
   - *Completely-new INSERT-only.* The plan is *pure-insert*: it contains at least one `INSERT` and
     no operator that reads or binds existing graph state (no scan/index/expand/match and no
     `SET`/`REMOVE`/`DELETE`), so every edge endpoint is a freshly inserted vertex and the whole
     plan needs no anchor or seeds. Federated placement requires a target shard for these brand-new
     (unanchored) elements; the policy is **place a completely-new INSERT on the graph's latest
     shard** (the live shard with the greatest graph-local `shard_id`; shard ids grow densely
     `0..n-1`). Such a plan — single- or multi-statement — is routed to the latest shard and
     executed there atomically. This also enables a single unanchored `INSERT` on a federated graph
     (previously rejected with `no index anchor`).
     `detection: gleaph_gql_planner::PhysicalPlan::is_pure_insert`.
   - *Anchored single-shard.* The plan is a *single-anchor threaded bundle*: it reads existing graph
     state in exactly one place — a single leading index/label anchor the Router can resolve to a
     shard set — and every later operator only mutates threaded bindings, inserts new elements, or
     reshapes already-bound rows (no second scan, traversal, join, or sub-plan that reaches back
     into the graph). Such a bundle performs no cross-shard reads, so when its leading anchor
     resolves to a single shard the whole multi-statement program runs there atomically. The Router
     admits it past the pre-dispatch gate and, after resolving the anchor, rejects it with
     `RouterError::UnsupportedMultiDmlBundle` only if the anchor actually spans more than one shard.
     A single DML statement that fans out to many shards remains the Phase 4 roll-forward saga.
     `detection: gleaph_gql_planner::PhysicalPlan::is_single_anchor_threaded_bundle`. This subset is
     enforced for ad-hoc execution; prepared multi-DML on a federated graph stays rejected at
     registration (the runtime shard count is not known at registration time).

   MATCH-based bundles with a second scan, a traversal, or independent per-statement matches remain
   rejected, since their cross-shard reads have no defined partial-application contract.
2. **Roll-forward bundle** *(implemented):* a single-anchor threaded bundle whose leading anchor
   resolves to **more than one shard** is dispatched per shard as a roll-forward saga, generalizing
   the contract-1 anchored subset (which required the anchor to resolve to one shard) and reusing the
   Phase 4 saga machinery that already fans a single DML statement across shards. Because the bundle
   performs no cross-shard read (`is_single_anchor_threaded_bundle`), each shard runs the whole
   multi-statement program over its own anchor rows **atomically shard-locally** (§1); cross-shard
   convergence is **roll-forward, not all-or-nothing** — a shard that fails mid-bundle leaves the
   mutation non-terminal (`CanonicalPending`) with the already-committed shards durable, and the saga
   converges by idempotent retry (resumes only the outstanding shards, deduplicated by `mutation_id`)
   and by the Phase 4 recovery timer for projection. The per-plan/per-shard cursor is the existing
   `RouterMutationRecord` (per-shard `completed`/`projection_advanced`). The contract-1 dispatch-time
   rejection of a multi-shard anchor is removed; the pre-dispatch gate (which admits only pure-insert
   or single-anchor threaded bundles) is the single admission point, so a multi-DML bundle reaching
   dispatch on a federated graph is structurally guaranteed to have no cross-shard read. Ad-hoc
   execution only; prepared multi-DML on a federated graph stays rejected at registration. Partial
   cross-shard visibility is possible while a saga is mid-flight; this is the explicit semantic
   promise (no global rollback).
3. **Staged distributed commit:** reserve for a named cross-shard all-or-nothing requirement.

Tests:

- Router rejects unsupported multi-DML before any shard dispatch. *(Done:
  `router_rejects_federated_match_based_multi_dml_bundle_before_dispatch`,
  `router_allows_multi_dml_bundle_on_single_shard`, and
  `program_modification::count_dml_statements_*` host unit tests.)*
- A supported one-shard bundle executes atomically on a single shard. *(Done for the completely-new
  INSERT-only subset: `router_places_completely_new_single_insert_on_latest_shard`,
  `router_places_completely_new_insert_bundle_on_latest_shard`, and `is_pure_insert_*` host unit
  tests. Done for the anchored single-shard subset:
  `router_runs_anchored_multi_dml_bundle_when_anchor_resolves_to_one_shard` and
  `is_single_anchor_threaded_bundle_*` host unit tests.)*
- A supported bundle that fans out to many shards runs per shard as a roll-forward saga, and a retry
  resumes at the persisted per-shard boundary without being described as rollback atomicity. *(Done
  for the single-anchor threaded subset: `router_runs_anchored_multi_dml_bundle_across_shards_as_roll_forward_saga`
  asserts both shards apply the bundle and the saga converges to `Completed`;
  `router_recovers_anchored_multi_dml_roll_forward_saga_via_idempotent_retry` crashes a shard
  mid-bundle, asserts `CanonicalPending`, that the autonomous timer leaves the canonical write pending
  without double-applying, and that restart plus idempotent retry converges to `Completed`.)*

Benchmarks:

- Bundle validation and one-shard execution at representative statement counts. **Done.** One-shard
  execution of an N-statement INSERT bundle in a single canonical segment is a graph canbench at N = 4
  and N = 16 (`bench_graph_canonical_segment_insert_bundle_4` / `_16`, ~672K / ~1.32M instructions —
  near-linear in statement count over fixed segment overhead; `crates/graph/canbench_results.yml`).
  The multi-DML admission gate cost (`count_dml_statements` + `is_pure_insert` /
  `is_single_anchor_threaded_bundle`) is a gql-planner criterion bench at N = 1/4/16/64
  (`bundle_validation`, `crates/gql-planner/benches/planner_bench.rs`).

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
