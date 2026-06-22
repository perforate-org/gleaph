# 0029. Shard-local atomicity and asynchronous cross-canister consistency

Date: 2026-06-21
Status: accepted
Last revised: 2026-06-21

## Context

An Internet Computer canister commits one message-handler execution atomically. A trap rolls back
the changes made by that handler. An inter-canister call introduces a commit and interleaving point:
code before and after `await` does not form one atomic transaction.

Gleaph already uses both sides of this execution model:

- a graph shard owns canonical vertices, edges, properties, and graph-local mutation outcomes;
- graph-index owns derived property and label postings;
- Router owns orchestration, client-key idempotency, and the label-stats projection;
- graph mutation journals, label-stats delta logs, and index repair journals make retries and
  asynchronous repair durable.

The implementation therefore has strong local durability and idempotent recovery, but the word
`transaction` in a GQL program currently spans boundaries with different guarantees:

- one mutation plan on one graph shard is applied inside one atomic message segment;
- multiple DML plans are separated by asynchronous index flushes;
- Router dispatches a federated mutation to graph shards sequentially, without cross-shard rollback;
- a repair-journaled index flush may leave graph-index behind canonical graph state after the
  mutation has been reported successful;
- a read-only federated query has no shared snapshot timestamp across shards.

ADR 0023 established store-ahead/index-repaired convergence. ADR 0024 made a durable canonical
mutation completable even when index repair is pending. Those are intentional distributed-system
properties, but they do not constitute a cluster-wide ACID transaction and need one explicit
consistency contract.

## Decision

### 1. Define the atomicity boundary at the canonical owner

The supported atomic write boundary is:

> One canonical mutation segment executed by one graph shard without an inter-canister commit
> point inside that segment.

The graph shard is the sole owner of canonical graph data and its local invariants. Validation,
canonical writes, the graph mutation-journal transition, and durable projection intent must be
committed in the same message segment where the relevant invariant requires all-or-nothing behavior.

Code must fetch remote inputs before entering the canonical mutation segment. If a mutation reads
state before an `await` and writes after it, the write boundary must revalidate an owner-controlled
revision or equivalent precondition.

### 2. Treat cross-canister state as an asynchronous projection

Property postings, label postings, and Router label statistics are derived from canonical graph
state. They are not participants in the graph shard's local atomic transaction.

The consistency mechanism is durable, ordered, idempotent propagation:

1. the canonical owner records projection intent durably;
2. delivery may occur more than once;
3. the consumer applies an event idempotently;
4. a durable cursor or mutation-linked watermark records the contiguous applied prefix;
5. repair resumes after rejection, trap, upgrade, or temporary unavailability.

Existing specialized mechanisms remain the starting point:

- `LABEL_STATS_DELTA_LOG` plus `ROUTER_LABEL_STATS_PROJECTION` for label statistics;
- the graph index repair journal for failed property/label posting flushes;
- graph and Router mutation journals for mutation outcome recovery.

This ADR does not require one generic event bus. Shared envelopes or cursor abstractions may be
introduced only when they remove duplicated invariants without obscuring the different payload and
retention contracts.

### 3. Separate canonical commit from projection completion

The target mutation lifecycle is:

```text
Routing
  -> CanonicalPending
  -> CanonicalCommitted
  -> ProjectionPending
  -> Completed
```

`Failed` is terminal only when no canonical write committed. A mutation with any committed
canonical write is recoverable and must remain in a roll-forward state rather than being relabeled
as an all-or-nothing failure.

The terms mean:

| Phase | Meaning |
|-------|---------|
| `Routing` | Router is resolving and durably recording the immutable dispatch envelope |
| `CanonicalPending` | At least one required canonical shard outcome is not yet known |
| `CanonicalCommitted` | All required canonical shard writes are durable |
| `ProjectionPending` | Canonical writes are durable; one or more required derived projections lag |
| `Completed` | Canonical writes and the projections required by the mutation contract reached their watermarks |
| `Failed` | Validation or execution failed before any canonical write committed |

During migration, the existing graph-journal `Completed` value continues to mean "the shard-local
canonical mutation outcome is replayable". It must not be interpreted as proof that every
cross-canister projection has converged.

### 4. Use an idempotent Router saga for federated mutations

Router owns the cross-shard execution flow. It persists an immutable mutation envelope before the
first shard dispatch, including the request fingerprint, resolved catalogs, target shards, and seed
bindings required for deterministic replay.

Each graph shard applies the same `mutation_id` idempotently. Router records per-shard canonical and
projection progress and retries incomplete work. Recovery must not depend solely on the original
client retrying.

This is a roll-forward saga, not a distributed rollback protocol. Reads that require a globally
committed view must use the visibility rules in the next section.

**Implementation (Phase 4) — autonomous, projection-only recovery.** Recovery is split by risk:

- *Liveness (autonomous, projection-only).* A single self-rescheduling `ic-cdk-timers` timer
  (`crates/router/src/recovery.rs`), armed after every idempotent DML and re-armed from
  `init` / `post_upgrade`, scans a bounded slice of `ROUTER_MUTATION_BY_CLIENT_KEY` per tick for
  non-terminal sagas that already have a persisted dispatch envelope, and drives each forward with
  **idempotent, cursor-guarded projection/index convergence only** (`gql::recover_mutation_record`
  → label-stats projection advance + graph-index watermark check). The driver **never re-dispatches
  canonical DML**: autonomous re-execution of a shard write is the one operation that risks
  double-apply, so it is deliberately excluded from the background path. Per-tick work and scan
  budget are bounded; the timer backs off and stops when a lap finds no recoverable saga.
- *Unfinished canonical writes (`CanonicalPending`).* Resumed by **explicit retry**, not the timer:
  re-presenting the same `client_mutation_key` resumes the saga idempotently via the inline
  reconciliation path. `mutation_status` reports `next_action` so a client / SDK / operator knows a
  retry is required.
- *Stuck routing reservation.* `routing_in_progress` now carries a lease (`routing_lease_ns`,
  `ROUTING_LEASE_TTL_NS`). A retry may reclaim a reservation whose lease has expired; this is safe
  because `routing_in_progress == true` implies the immutable envelope was not yet persisted and
  therefore no canonical write has happened.
- *Retention safety.* TTL eviction (ADR 0025 B + sweep) now reclaims **only terminal** records, so a
  non-terminal saga is never discarded before recovery finishes (ADR 0025, "Eviction predicate
  revision").
- *Observability is pull-based.* `mutation_status(logical_graph_name, client_mutation_key)` (router
  `#[query]`) returns the lifecycle phase, last recovery diagnostic, outstanding target shard, and
  next action. Read-your-writes convergence is observed through `AtLeast(token)` reads (§5) and this
  query; the timer never returns results to a client.

Schema: `RouterMutationRecord` gains `routing_lease_ns: Option<u64>` and `last_error: Option<String>`
(both Candid `opt`, so old region-7 records decode as `None` with no migration).

### 5. Make read consistency explicit

The API distinguishes (`gleaph_graph_kernel::plan_exec::ReadMode`):

| Read mode | Contract | Status |
|-----------|----------|--------|
| `Eventual` | Read the available derived projection; it may lag canonical state | Implemented (Phase 3; default) |
| `AtLeast(mutation_token)` | Succeed only after every required projection reaches the token's watermark; otherwise return a retryable lag result | Implemented (Phase 3) |
| `Canonical` | Read from the canonical graph owner where the query shape supports it | Deferred (Phase 3; rejected at runtime, not downgraded) |

A mutation token identifies the mutation and the shard-local projection watermarks required for
read-your-writes. The token is not a global MVCC snapshot timestamp.

Count-only Router projections and graph-index-backed membership/property reads must not silently
claim read-your-writes unless their respective cursor or watermark has reached the token.

**Implementation (Phase 3).** Callers select a mode via the router composite-query entrypoints
`gql_query_with_consistency` / `prepared_execute_query_with_consistency`; the legacy `gql_query` /
`prepared_execute_query` stay `Eventual`. The barrier is enforced once before any read shape is
dispatched (label-count fast path, graph-index seed, and graph-shard scan are gated uniformly).
For `AtLeast(token)`, each token shard must satisfy its label-stats projection cursor
(`label_stats_projection_cursor`) and its graph-index watermark (`index_pending_min_mutation_id`,
index-satisfied iff `None` or `mutation_id < value`); an unmet watermark returns retryable
`RouterError::ProjectionLag` without serving stale state. `Canonical` is reserved on the wire but
rejected (`InvalidArgument`) until owner-side scan routing and the unsupported-shape catalog land.

### 6. Restrict multi-DML until its boundary is explicit

Federated multi-DML programs are not advertised as atomic. The first implementation phase
(**implemented**, Phase 5 rejection gate) rejects unsupported multi-DML update bundles at the
Router boundary rather than preserve a syntax that suggests rollback semantics the system does not
provide.

The gate rejects a write program iff its statement block holds **more than one top-level DML
statement** (`StatementBlock::first` plus each `NEXT` statement; a single statement counts once
regardless of how many DML parts it contains) **and** the target graph is **federated** (more than
one live shard). It returns `RouterError::UnsupportedMultiDmlBundle` before resolving seeds or
dispatching to any shard, so no canonical or projection state changes. The top-level DML statement
count is derived from the AST (`gleaph_gql::program_modification::count_dml_statements`, the source
of truth for how many DML statements the program contains). Single-shard multi-DML stays
shard-local atomic (decision 1) and a single federated DML statement converges via the federated
saga (decision 4), so both pass. The gate is enforced at both AST-owning ingress points: ad-hoc
`gql_query*`/`gql_update*` (`router::gql::run_gql`) and prepared-plan registration
(`router::prepared::prepared_register`), so a federated multi-DML prepared plan is never persisted.
A prepared plan registered against a single-shard graph that is later re-sharded is an orthogonal
prepared-plan staleness concern, not covered by this gate.

**Contract 1 (one-shard atomic bundle), completely-new INSERT subset — implemented.** A bundle
that is *pure-insert* (contains at least one `INSERT` and no operator that reads or binds existing
graph state — no scan/index/expand/match, and no `SET`/`REMOVE`/`DELETE`) creates only brand-new
elements, so every edge endpoint is a freshly inserted vertex and the plan needs no index anchor or
seeds. The Router places such a bundle on the graph's **latest shard** — the live shard with the
greatest graph-local `shard_id` (shard ids grow densely `0..n-1` via `next_graph_local_shard_id`) —
and executes the whole plan there. The shard's existing single canonical critical section (§1)
applies all of the bundle's statements atomically and provides read-your-own-writes between them
(e.g. `INSERT (a) NEXT INSERT (a)-[:E]->(b)`), since they are co-located. This is the placement
authority for brand-new federated elements (the Router owns placement; graph shards do not). It
also resolves a prior gap: a single unanchored `INSERT` on a federated graph used to fail with
`no index anchor: single-shard graph required`; it is now placed on the latest shard.
`detection: gleaph_gql_planner::PhysicalPlan::is_pure_insert`.

**Contract 1 (one-shard atomic bundle), anchored single-shard subset — implemented.** A bundle that
is a *single-anchor threaded bundle* reads existing graph state in exactly one place — a single
leading index/label anchor the Router can resolve to a shard set by index lookup — and every later
operator only mutates threaded bindings, inserts new elements, or reshapes already-bound rows (no
second scan, traversal, join, or sub-plan that reaches back into the graph). Because the only
existing data the bundle touches is the leading anchor's rows, it performs **no cross-shard reads**:
when the anchor resolves to a single shard, the whole multi-statement program can run on that shard
under the shard's single canonical critical section (§1), which applies every statement atomically
and provides read-your-own-writes between them. The Router admits such a bundle past the pre-dispatch
gate and resolves the leading anchor; when it resolves to one shard the bundle runs there atomically.
`detection: gleaph_gql_planner::PhysicalPlan::is_single_anchor_threaded_bundle`. This subset is
enforced for ad-hoc execution; the runtime shard count is unknown at prepared-plan registration, so
prepared multi-DML on a federated graph stays rejected at registration (the orthogonal
re-sharding-staleness caveat above still applies). MATCH-based bundles with a second scan, a
traversal, or independent per-statement matches remain rejected by the §6 gate, since their
cross-shard reads have no defined partial-application contract.

**Contract 2 (roll-forward bundle), single-anchor threaded subset — implemented.** When the same
single-anchor threaded bundle's leading anchor resolves to **more than one shard**, the Router no
longer rejects it; it dispatches the whole bundle per shard as a roll-forward saga, generalizing the
contract-1 anchored subset and reusing the Phase 4 saga machinery that already fans a single DML
statement across shards (decision 4). Because the bundle performs no cross-shard read, each shard
runs the entire multi-statement program over its own anchor rows atomically shard-locally (§1).
Cross-shard convergence is **roll-forward, not all-or-nothing**: a shard that fails mid-bundle leaves
the mutation non-terminal (`CanonicalPending`) with the already-committed shards durable; the saga
converges by idempotent retry (resuming only the outstanding shards, deduplicated by `mutation_id`)
and by the Phase 4 recovery timer for projection. The per-plan/per-shard cursor is the existing
`RouterMutationRecord` (per-shard `completed` / `projection_advanced`). With this contract the
contract-1 dispatch-time rejection of a multi-shard anchor is removed: the pre-dispatch §6 gate —
which admits only pure-insert or single-anchor threaded bundles — is the single admission point, so a
multi-DML bundle reaching dispatch on a federated graph is structurally guaranteed to have no
cross-shard read, and no count-based runtime gate is needed (a single DML statement fanning out stays
the decision-4 saga, unchanged). Partial cross-shard visibility is possible while a saga is
mid-flight; this is the explicit semantic promise (no global rollback). Ad-hoc execution only;
prepared multi-DML on a federated graph stays rejected at registration.

The remaining (cross-shard-reading) cases may later use staged writes and a commit protocol for
operations that genuinely require cross-shard all-or-nothing visibility (§7).

Per-plan progress journaling improves roll-forward recovery but does not by itself provide
Atomicity and must not be described as ACID.

### 7. Add stronger protocols only for named invariants

Cross-shard TCC, staged commit, MVCC, or optimistic conflict validation may be introduced for a
specific invariant such as uniqueness, quota reservation, schema publication, or a conditional
compare-and-set. They are not the default for all GQL mutations.

Every stronger protocol requires its own ADR or an amendment that names:

- canonical owner and staged state;
- prepare, commit, cancel, and recovery transitions;
- read visibility while prepared;
- timeout behavior without unsafe lease expiry;
- upgrade/reopen behavior and bounded retention;
- conflict and retry semantics.

### 8. Preserve the boundary when the graph canister is split into more shards

Splitting the graph canister into additional shards, and introducing shard-to-shard `await`, does
not relax the boundary in §1. The boundary is *shard-local by definition*, and the IC execution
model makes that durable rather than fragile:

- An inter-canister `await` is a commit and interleaving point, so a single critical section that
  spans two shards is physically impossible. There is no future call mechanism that turns two
  shards' writes into one atomic message segment.
- Therefore cross-shard atomicity is never obtained by extending a critical section across an
  `await`. It is *composed* from multiple shard-local atomic segments coordinated above them:
  - the default path is the idempotent roll-forward Router saga (§4); and
  - a named strong invariant (§7) uses a prepare/commit/cancel protocol in which **each** shard's
    prepare and **each** shard's commit is itself a separate shard-local atomic segment.
- Remote inputs — including inputs sourced from peer shards once sharding exists — are fetched
  before the segment (§1). The canonical mutation segment must take no cross-canister client handle
  (graph-index or peer-shard); a cross-shard `await` belongs in the coordination layer between
  segments, never inside one.

Enforcement note. Today the boundary is enforced structurally but narrowly: the canonical mutation
segment is constructed without a `PropertyIndexLookup` handle and runs `CALL` procedures
synchronously, so it cannot reach the only existing inter-canister paths. When a peer-shard client
is introduced, that narrow construction is no longer sufficient on its own. Enforcement must then
generalize to a path-independent guard — assert "no canonical segment is active" at every
inter-canister chokepoint (graph-index client, Router call, and any peer-shard client) — so a new
call path added inside the segment fails loudly instead of silently extending the critical section
across a commit point. This guard is expected when a second inter-canister path first appears
(peer-shard client or the Phase 4/6 cross-shard coordination work), not before.

## Invariants

| Invariant | Owner | Enforcement point |
|-----------|-------|-------------------|
| Canonical graph state changes atomically within the supported local boundary | Graph | Synchronous canonical mutation segment |
| The canonical mutation segment carries no inter-canister call/commit point, including after graph-shard splitting | Graph | Segment constructed without a cross-canister client handle today; path-independent "no active segment" guard once a peer-shard client exists |
| A committed canonical mutation has durable replay/repair metadata before cross-canister work can be lost | Graph | Mutation journal and projection-intent write boundary |
| One client key and fingerprint reuse one mutation identity | Router | Client mutation reservation |
| One mutation id is not applied twice on a graph shard | Graph | Graph mutation journal lookup before execution |
| Router saga progress is monotonic and replayable | Router | Per-shard mutation record transitions |
| Derived consumers apply an ordered prefix idempotently | Router or graph-index | Projection apply plus durable cursor/watermark |
| `Completed` at the Router means all contract-required canonical and projection work completed | Router | Final mutation transition |
| Unfinished canonical work is not removed by ordinary completed-record retention | Owning journal | Retention eligibility check |

## Consequences

### Positive

- The design uses IC message atomicity where it is strongest instead of emulating intra-canister
  locks.
- Canonical ownership remains on graph shards; Router and graph-index do not acquire graph storage
  authority.
- Existing journals and projection logs become parts of one explicit consistency model.
- Clients can distinguish durable canonical success from projection freshness.
- Stronger distributed transaction machinery is limited to invariants that justify its cost.

### Trade-offs

- Eventual reads may remain stale unless the caller supplies a mutation token or requests a
  canonical read.
- A federated mutation can remain partially applied while its saga is unfinished.
- Router needs an autonomous recovery driver and operator-visible mutation status.
- Mutation-result and read-consistency APIs will change.
- Rejecting multi-DML temporarily narrows supported GQL update programs.

## Alternatives considered

### Keep the current implicit contract

Rejected. The implementation is recoverable, but `Completed`, client success, and projection
freshness currently have overlapping meanings. The ambiguity is visible to clients and design
documents.

### Serialize every update with one graph-wide lock

Rejected as the target design. It is a useful emergency safety mechanism but discards concurrency
between independent shards and does not make cross-canister effects atomic. A durable lock held
across `await` also introduces recovery and liveness problems.

### Use distributed locks or range locks for all updates

Rejected. IC already serializes each canister's message handlers. Cross-canister locks add blocking,
deadlock, lease, and coordinator-failure concerns while projections still require repair.

### Introduce cluster-wide MVCC and two-phase commit now

Rejected for the general path. It would add versioned canonical storage, staged writes, a timestamp
authority, prepared-state retention, read snapshot propagation, and coordinator recovery before a
demonstrated product requirement justifies them.

### Accept untracked eventual consistency

Rejected. Eventual consistency is acceptable only when propagation intent is durable, application
is idempotent, progress is observable, and a recovery path exists.

## Implementation plan

The phased implementation and acceptance gates are defined in
[ACID and consistency roadmap](../architecture/acid-roadmap.md).

## Related decisions

- [ADR 0015: Label stats projection log](0015-label-stats-projection-log.md)
- [ADR 0023: Federated index/store consistency](0023-federated-index-consistency-upgrade-compaction.md)
- [ADR 0024: Mutation journal completion vs index flush](0024-mutation-journal-completion-vs-index-flush.md)
- [ADR 0025: Client mutation journal retention](0025-client-mutation-journal-retention-sweep.md)
- [ADR 0027: Graph mutation journal retention](0027-graph-mutation-journal-retention.md)
