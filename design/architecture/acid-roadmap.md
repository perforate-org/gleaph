# Gleaph ACID and Consistency Roadmap

Last updated: 2026-06-21 UTC
Status: Planned
Anchor timestamp: 2026-06-21 05:36:08 UTC +0000

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

**Status: Planned.**

Goal: remove ambiguous uses of transaction success before changing persistence or APIs.

Deliverables:

- Implement the accepted ADR 0029 contract through the phases below.
- Define `CanonicalCommitted`, `ProjectionPending`, and `Completed` in public/internal contracts.
- Clarify that existing graph-journal `Completed` means shard-local replayable outcome, not global
  projection freshness.
- Reconcile ADR 0023's completion invariant with ADR 0024's deferred-index success.
- Document the supported consistency mode of every update and index-backed read entrypoint.
- Decide whether public mutation APIs return a richer result or add a status/token endpoint.

Tests:

- Characterization tests for current mutation-journal and projection transitions.
- A contract test that prevents Router `Completed` while a required shard/projection is unfinished.

Exit criteria:

- No active design document equates canonical commit with cross-canister convergence.
- `Completed` has one owner and one meaning at each boundary.

## Phase 1: Protect the local atomic boundary

**Status: Planned.**

Goal: make the Graph critical section visible in code and tests.

Deliverables:

- Separate remote input acquisition from canonical mutation execution.
- Ensure the canonical mutation segment contains no inter-canister call/commit point.
- Commit canonical data, mutation outcome/progress, and required projection intent together.
- Add owner-controlled revision revalidation to any read-before-`await`/write-after-`await` path.
- Keep GraphStore domain commits as the only write path for affected invariants.

Tests:

- Trap injection after each local write step proves whole-message rollback.
- Reopen tests prove canonical state and mutation/projection intent survive together.
- Interleaving tests prove stale preconditions are rejected rather than silently overwritten.

Benchmarks:

- Graph mutation canbench for vertex/property/edge paths before and after boundary changes.

Exit criteria:

- Every supported shard-local DML either commits all owner-local state or commits none.
- No remote call occurs inside the named canonical critical section.

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
