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
client retrying; planned work adds a bounded timer/admin recovery driver.

This is a roll-forward saga, not a distributed rollback protocol. Reads that require a globally
committed view must use the visibility rules in the next section.

### 5. Make read consistency explicit

The target API distinguishes:

| Read mode | Contract |
|-----------|----------|
| `Eventual` | Read the available derived projection; it may lag canonical state |
| `AtLeast(mutation_token)` | Succeed only after every required projection reaches the token's watermark; otherwise return a retryable lag result |
| `Canonical` | Read from the canonical graph owner where the query shape supports it |

A mutation token identifies the mutation and the shard-local projection watermarks required for
read-your-writes. The token is not a global MVCC snapshot timestamp.

Count-only Router projections and graph-index-backed membership/property reads must not silently
claim read-your-writes unless their respective cursor or watermark has reached the token.

### 6. Restrict multi-DML until its boundary is explicit

Federated multi-DML programs are not advertised as atomic. The first implementation phase will
reject unsupported multi-DML update bundles at the Router boundary rather than preserve a syntax
that suggests rollback semantics the system does not provide.

Future support may choose one of two explicit contracts:

- execute all canonical DML for one shard in one message segment and publish projection intent at
  the end; or
- use staged writes and a commit protocol for operations that genuinely require cross-shard
  all-or-nothing visibility.

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
