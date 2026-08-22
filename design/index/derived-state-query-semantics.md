# Derived-state query semantics

Last updated: 2026-08-21
Anchor timestamp: 2026-08-21 22:45:03 UTC +0000

## Status

**Implemented** — documents current query behavior when derived indexes, label stats projection, or maintenance
state may lag canonical graph data. Complements [stable-memory-inventory.md](../storage/stable-memory-inventory.md).

## Purpose

State honestly what federated and standalone queries observe when derived stores are incomplete,
stale, or unavailable. Derived state is never consulted to recover canonical data; query paths must
not paper over sync gaps with graph-side tombstone filtering at the index layer.

## Migration-driven index activation

[ADR 0059](../adr/0059-create-index-migration-backfill.md) is the normative source for
migration-driven `CREATE INDEX` backfill and is **implemented** (durable Router lifecycle, Graph
canonical export scopes, graph-index build worker/state, pre-canonical seal fence, Graph
label-transition admission, and the production Router cross-canister driver with seal/drain
composition); the cross-canister PocketIC convergence/fence/upgrade-reopen proof landed via
GAP-2026-07-29-006 (closed 2026-08-22), while edge `INLINE` enumeration remains open under
GAP-2026-07-29-001. Its online pull,
`PhysicalIndexId`, touched-first outbox, seal, and Active-only planner rules therefore change this
document's activation semantics for migration-created indexes: only an `Active` generation is
planner-visible, while operator-driven backfill still does not prove activation or historical
completeness. A Graph label gain/loss selects exact `(label_id, property_id)` namespaces, emits
Building work before the canonical label change, dispatches Active work through the ordinary queue,
and rejects Sealing before mutation; the cross-canister convergence and upgrade/reopen proof is
still the release gate.

## Principles

1. **Canonical wins.** Vertex rows, properties, labels, and forward edges are authoritative on the
   graph shard. Derived stores are projections for read optimization.
2. **No index-side tombstone sieve.** Property and label index reads do not re-check live vertex
   existence on the graph shard ([standalone-mode.md](../sharding/standalone-mode.md)). Stale
   postings are a sync or backfill problem, not a query-time filter.
3. **Intentional index-only miss ≠ staleness.** Unindexable or null property values are omitted by
   design ([property-index.md](property-index.md)); equality/range scans will not find them without a
   full-scan path. Encoded index keys longer than `MAX_INDEX_VALUE_KEY_BYTES` (4096) are treated as
   non-indexable on write and rejected on index read/query derivation — not as stale postings.
4. **Maintenance cursors are not data.** Router `BackfillShardState` and graph pending queues track
   repair progress; they must not be read as membership or count truth.
5. **Canonical success is not a freshness barrier.** A graph mutation may be durable while a
   cross-canister projection is pending. Idempotent mutations report a `MutationLifecyclePhase`
   ([ADR 0029](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md) Phase 0,
   implemented) so a client can distinguish a durable canonical commit
   (`CanonicalCommitted` / `ProjectionPending`) from full convergence (`Completed`). Idempotent
   mutations also return a `MutationToken` (`GqlQueryResult.token`, ADR 0029 Phase 2, implemented)
   carrying the per-shard read-your-writes watermarks: each shard's label-stats `emitted_delta_last_seq`
   and — keyed by the monotonic `mutation_id` — the Graph-owned combined graph-index floor exposed
   by the graph query `index_pending_min_mutation_id`. The read-side `AtLeast(token)` barrier that _enforces_ these
   watermarks is implemented (ADR 0029 Phase 3): `gql_query(query, params, ReadMode::AtLeast(token))` serves the read only once every token shard has reached both
   watermarks, otherwise it returns a retryable `RouterError::ProjectionLag` without serving stale
   state.

The Graph-owned mutation-linked floor is the first key in MemoryId 52
`INDEX_PENDING_FLOOR`. Each qualifying durable source row contributes one fixed key ordered by
nonzero mutation id, owner (`RepairJournal` or `DerivedIndexOutbox`), and source sequence. Pending
and quarantined ordinary outbox rows participate; mutation id `0` and `IndexBuildDml` do not.
GraphStore prevalidates and synchronously co-updates owner regions 41/46 with the derived floor, so
duplicate mutation ids retain multiplicity and an out-of-order drain exposes the exact successor.

## Entrypoint consistency modes (ADR 0029)

The supported consistency contract of every public router GQL entrypoint (ADR 0056 §3
naming). The read-side `AtLeast(token)` barrier from
[ADR 0029](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md)
§5 is **implemented** (Phase 3) via the explicit `ReadMode` argument on `gql_query` and
`prepared_query`. `Eventual` remains the default contract (no per-read watermark); callers opt
into the barrier with `ReadMode::AtLeast(token)`. `ReadMode::Canonical` was removed in ADR 0056
(it was never implemented); `Eventual` and `AtLeast(token)` remain.

| Router entrypoint | Call kind         | Program                                         | Consistency contract                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ----------------- | ----------------- | ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `gql_query`       | query (composite) | read-only                                       | **Eventual** for projection/index-backed shapes (count-only may under-count; postings may lag — see Sync vs lag policy); graph-shard-served shapes (vertex/edge rows, property reads). `ReadMode` argument: `Eventual` matches the legacy default; `AtLeast(token)` enforces the read-your-writes barrier: served only once every token shard reaches its label-stats cursor and graph-index watermark, else retryable `RouterError::ProjectionLag` (no stale state).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `gql_mutate`      | update            | DML (idempotent); non-DML (index / catalog DDL) | Canonical write is shard-local atomic (ADR 0029 §1). A bundle with more than one top-level DML statement on a **federated** (multi-shard) graph is rejected with `RouterError::UnsupportedMultiDmlBundle` (ADR 0029 §6, Phase 5) unless it is structurally free of cross-shard reads: **(a)** completely-new INSERT-only (`is_pure_insert`, contract 1) — placed on the graph's latest shard and executed atomically there; or **(b)** a single-anchor threaded bundle (`is_single_anchor_threaded_bundle`: one leading index/label anchor, no other existing-state read) — when its leading anchor resolves to one shard the whole bundle runs there atomically (contract 1), and when it fans out to many shards it is dispatched per shard as a **roll-forward saga** (contract 2): each shard atomic shard-locally, cross-shard convergence roll-forward (no global rollback; partial visibility possible mid-saga), resumed by idempotent retry and the Phase 4 recovery timer. The pre-dispatch gate is the single admission point, so a multi-DML bundle reaching dispatch is structurally guaranteed safe. Single-shard multi-DML and single-statement federated DML (the saga) are also accepted. A completely-new single `INSERT` on a federated graph is likewise placed on the latest shard (previously rejected with `no index anchor`). Returns `GqlQueryResult.phase` (`MutationLifecyclePhase`). The label-stats projection is advanced **inline**, or the DML fails with `label stats projection lag`; graph-index postings may be deferred (ADR 0023/0024), leaving the federated mutation `ProjectionPending` until the durable outbox/repair owners drain. `Completed` means the canonical writes **and** the projections required by the mutation contract converged. Non-DML (index / catalog DDL) changes apply synchronously within the call (router is index-definition SSOT; `DROP INDEX` posting purge is driven to `done`). |
| `atomic_insert`   | update            | bounded typed vertex/edge insertion             | One request maps to one Graph shard and one shard-local atomic Graph request. The durable receipt contains graph-scoped encoded vertex IDs in vertex-operation ordinal order; edge-only receipts contain no IDs. A later edge-only `atomic_insert` may use a returned ID before Property Index convergence because Graph validates canonical vertex existence directly. `atomic_insert_status` recovers the same receipt after response loss while the terminal Router record remains inside its seven-day `terminal_at_ns` retention window.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `bulk_load`       | update            | resumable initial-load lifecycle                | One job is fixed to one graph and one target shard. Accepted chunks commit as a durable prefix and return vertex receipts in operation order; the complete job is not atomic. Exact append replay returns the persisted receipt, `bulk_load_status` exposes the next required chunk and paged receipts, and finalization completes only after canonical writes and required derived projections converge. Edge chunks address vertices through receipt-derived encoded IDs rather than Property Index lookup.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `prepared_mutate` | update            | DML (idempotent)                                | Same as `gql_mutate`, for a registered prepared plan. The federated multi-DML gate (ADR 0029 §6) runs at `prepare`, so a federated multi-DML plan can never be stored or reach this entrypoint. The contract-1 anchored single-shard subset is **ad-hoc only**: the leading anchor's runtime shard count is unknown at registration, so anchored multi-DML on a federated graph stays rejected at `prepare`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |

Read-your-writes today: after a `Completed` idempotent DML, label-stats count-only reads are
read-your-writes (the projection is drained inline before completion). Index-backed shapes
(membership, property equality/range) can still lag while a mutation is `ProjectionPending` — and
even after the Router-saga `Completed`, since the saga's `projection_advanced` tracks label stats
only, not graph-index posting delivery (which may be deferred to either durable Graph owner). The
returned `MutationToken` now lets a caller detect this: the Graph-owned combined graph-index floor
(`index_pending_min_mutation_id`) is keyed by `mutation_id` independently of the saga phase. The
Phase 3 `AtLeast(token)` read barrier makes the token enforceable per read: a caller that issues
`gql_query(.., ReadMode::AtLeast(token))` is either served read-your-writes
(every shard caught up) or gets a retryable `ProjectionLag`, never a silently stale projection.

## Sync vs lag policy

| Derived store                         | Update contract                                                                                                                   | Acceptable lag                                                               | Query impact when lagging                                                                                                                                                                                                    |
| ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Edge property postings (graph-index)  | **Async** via `edge_pending` flush on federated DML                                                                               | graph-index may lag canonical                                                | Expand equality may miss until backfill; use `backfill_edge_property_postings`                                                                                                                                               |
| Edge aliases                          | **Sync** on edge insert/delete                                                                                                    | None (bug if mismatched)                                                     | Wrong reverse/undirected expand; use `check_edge_aliases` / `rebuild_edge_aliases`                                                                                                                                           |
| Property postings (graph-index)       | DML enqueue + `pending` flush                                                                                                     | Pending queue before flush; flush retry; historical **backfill** in progress | **Under-posted:** equality/range/seed miss live vertices. **Over-posted:** extra hits until remove syncs. No silent drop at read time                                                                                        |
| Label postings (graph-index)          | DML enqueue + `label_pending` flush                                                                                               | Same as property postings                                                    | **Under-posted:** label sieve / export / intersection miss. **Over-posted:** extra hits until remove syncs                                                                                                                   |
| Router label stats projection         | Graph `LABEL_STATS_DELTA_LOG` replay via `advance_label_stats_projection` ([ADR 0015](../adr/0015-label-stats-projection-log.md)) | Unacked deltas in graph log; router down before drain; gap in delta log      | **Count-only** fast path may **under-count** (reads aggregates without cursor check). **DML** fails if projection cannot reach `emitted_delta_last_seq`. Vertex-list paths use label **postings**, not projection aggregates |
| Graph CSR vertex rows (tombstone bit) | Graph DML                                                                                                                         | Tombstone on delete; no slot reuse                                           | Live vertex = row in range and not tombstoned                                                                                                                                                                                |
| Index property/label postings         | Graph DML → index sync                                                                                                            | Backfill from graph                                                          | Stale posting = DML/index sync bug                                                                                                                                                                                           |

## Scenarios

### Pending queue not flushed

Graph DML enqueues posting ops in `index/pending.rs` / `label_pending.rs`. Until
`flush_pending` succeeds, graph-index lags canonical shard state. Mutations after enqueue are ordered
per shard; a failed flush batch is compensated and re-queued ([`pending.rs`](../../crates/graph/src/index/pending.rs)).

**Query behavior:** Index anchors and router seeds reflect last successful flush only. Operators
should not assume read-your-writes through the index until flush completes.

### No index client configured

Without a wired index client, graph may drop index maintenance on DML. Canonical stores still
update.

**Query behavior:** Index-backed plans return empty or fail at router dispatch depending on path.
This is a deployment misconfiguration, not a supported degraded mode.

### Backfill in progress

`backfill_label_postings` / `backfill_vertex_property_postings` replay historical canonical state into
graph-index. Router `advance_backfill(graph, kind, max_work)` advances per-shard cursors (`BackfillShardState`).

**Query behavior:** Vertices before the cursor may be missing from the index. DML after deployment
still flows through pending flush independently. Run backfill loops until `done` before relying on
historical completeness.

**Convergence signal:** `get_graph_sync_status(args)` (Router, `Role::Admin`)
returns the graph shard's durable derived-index backlog — the first-delivery outbox
(`derived_index_outbox_len`) plus the failed-flush repair journal (`repair_journal_len`) — with a
derived `converged` flag (`true` only when both are empty). Unlike the mutation-linked
`index_pending_min_mutation_id` floor, this snapshot reports raw all-work queue lengths (including
build envelopes and zero-id work), so a caller can wait until `converged` before dispatching
operations whose index anchors must see prior canonical writes (for example, the social-demo seed
waves). The cursor-driven backfill steps remain the repair path when convergence stalls.

For the implemented migration lifecycle, this signal is necessary but not sufficient by itself: the
generation-specific cursor, touched-subject set, catalog epoch, and seal watermark must all be
durably converged before Router publishes `Active`. `Preparing`, `Building`, `Sealing`, and
`Aborting` generations are never planner candidates; queries use the existing active generation or
a non-index fallback according to the current query contract.

### Label stats projection lag

Router label stats are an event-sourced projection ([ADR 0015](../adr/0015-label-stats-projection-log.md)).
Graph shards append `LabelStatsDelta` events to `LABEL_STATS_DELTA_LOG`; router aggregates land in
`ROUTER_VERTEX_LABEL_STATS`, `ROUTER_EDGE_LABEL_STATS`, and per-shard live maps
(`ROUTER_*_LABEL_LIVE_BY_SHARD`). `ROUTER_LABEL_STATS_PROJECTION` records each shard's
`applied_through_seq` — the durable idempotency boundary for ordered replay. All apply paths go
through `advance_label_stats_projection`; there is no full historical rebuild from vertex label scans.

**DML vs read asymmetry (operational):**

| Path                                                     | Projection contract                                                                                | Observable when lagging                                                                             |
| -------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| Federated **DML**                                        | After each shard execute, advance through `emitted_delta_last_seq` from the graph mutation journal | Mutation **fails** with `label stats projection lag for shard …` if deltas cannot be drained inline |
| **Read-only** `MATCH (n:L) RETURN count(*)` (path **B**) | Fast path reads `ROUTER_VERTEX_LABEL_STATS.live_count` directly                                    | Query **succeeds** with a potentially stale **under-count**; no cursor check at read time           |
| Vertex list / compound seeds (paths **A**, **C**, **D**) | graph-index label **postings**                                                                     | Unaffected by projection lag unless postings are separately stale                                   |

Normal DML therefore blocks new writes when projection cannot catch up; ad-hoc count queries do not
surface lag as an error. Operators who need count correctness after router downtime must drain
pending deltas before trusting count-only results.

**Advance invariants:**

- Per-shard cursor advances only over a **contiguous prefix** of `LABEL_STATS_DELTA_LOG`.
- A gap in the log fails advance with `label stats projection gap`; cursor and aggregates stay at
  the last good prefix until the graph supplies the missing seq.
- `admin_label_stats_projection_step` (Admin-only) loops `advance_label_stats_projection` with
  `max_deltas` per call; repeat until `done` when `deltas_applied < max_deltas`.
- Mutation retry uses the graph mutation journal (`emitted_delta_first_seq` /
  `emitted_delta_last_seq`) and `reconcile_router_mutation_projection` for shards that completed
  execution but did not yet record `projection_advanced`.

**Query shapes affected by lag:**

| Shape                                                  | Source                             | Lag symptom                          |
| ------------------------------------------------------ | ---------------------------------- | ------------------------------------ |
| `MATCH (n:L) RETURN count(*)` (no `GROUP BY` property) | Router projection aggregates       | Under-count                          |
| `MATCH (n:L) GROUP BY n.p` / property filter + label   | graph-index postings + label sieve | Not projection lag (see posting lag) |
| `MATCH (n:L) RETURN n`                                 | graph-index label postings         | Not projection lag                   |
| Edge label counts (if exposed)                         | Router edge projection aggregates  | Same under-count pattern as vertex   |

**Remediation checklist:**

1. Per affected shard: call `admin_label_stats_projection_step` in a loop until `done`.
2. If advance fails with **gap**, inspect graph `LABEL_STATS_DELTA_LOG` for the missing seq before
   retrying — do not expect aggregates to self-heal past a hole.
3. If deltas were acked and dropped while cursor lags, replay depends on graph retention policy;
   there is no router-side full rescan fallback.
4. After canister upgrade, projection cursors survive on router and the delta log survives on graph
   shards; drain before count SLA checks.

See also [label-index.md](label-index.md) path **B** and
[stable-memory-inventory.md](../storage/stable-memory-inventory.md) (router regions 25–29).

### Upgrade / ephemeral loss

Heap-only pending queues and router ephemeral planner catalogs are lost on upgrade ([stable-memory-inventory.md](../storage/stable-memory-inventory.md)). The Router direct-vector-ingestion outbox is stable and re-armed after `post_upgrade`; its recovery contract is documented in [vector-index.md](vector-index.md). Stable backfill cursors and projection cursors survive on router; graph delta log survives on shard.

**Query behavior:** Run label stats projection and posting backfill after upgrade when index or
count completeness is required.

## Operator expectations

| Symptom                                                            | Likely cause                                                                                    | Remediation                                                                                                         |
| ------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Index miss for known property value                                | Unindexable value, oversized encoded key (>4096 B), pending not flushed, or backfill incomplete | Check `property_indexability` and key size; flush pending; run property backfill                                    |
| Index-backed seed/`MATCH` misses a live vertex after a large batch | Batch drain deferred to the durable outbox or repair journal (ADR 0023/0024)                    | Poll `get_graph_sync_status` until `converged`; run `advance_backfill` (kind = VertexProperty / Label) if it stalls |
| Extra index hit for deleted vertex                                 | Remove posting not synced                                                                       | Flush/retry pending; verify DML index path                                                                          |
| `COUNT(*)` under-counts for label after router restart             | Projection lag on read path (DML would have failed instead)                                     | Drain `admin_label_stats_projection_step` per shard until `done`; verify cursor vs log head                         |
| DML fails with `label stats projection lag`                        | Inline advance could not reach journal `emitted_delta_last_seq`                                 | Drain projection for that shard; retry mutation                                                                     |
| DML fails with `label stats projection gap`                        | Missing seq in graph delta log                                                                  | Fix graph log continuity before advancing cursor                                                                    |
| Expand equality wrong                                              | graph-index edge posting lag or unregistered property                                           | `backfill_edge_property_postings`; verify index registry                                                            |
| Reverse expand wrong                                               | Edge alias drift                                                                                | `check_edge_aliases`; `rebuild_edge_aliases`                                                                        |

## Implementation-gap traceability (non-normative)

The implementation-gap ledger is the status authority for the following
observations. These links do not amend the query contracts above.

- [GAP-2026-08-20-001](../implementation-gaps.md#gap-2026-08-20-001--atleast-graph-index-barrier-ignores-pending-first-delivery-outbox-work) — **Resolved in this patch**: exact MemoryId 52 Graph-owned floor plus the passing outbox-only stopped-index Graph-upgrade barrier regression.
- [GAP-2026-08-20-002](../implementation-gaps.md#gap-2026-08-20-002--router-direct-vector-ingestion-durable-intent-ownership) — **Resolved**: MemoryId 53 owns both pre-Graph and pre-Vector phases. Focused unit and PocketIC gates cover exact replay and Router upgrade; they do not prove autonomous timer firing or watermark/tombstone GC completion.
- [GAP-2026-08-20-003](../implementation-gaps.md#gap-2026-08-20-003--canonicalpending-retry-does-not-reconcile-a-completed-graph-receipt) — **Resolved in `d700331c`**: exact completed-receipt adoption and trigger-aware `Absent` handling; focused Router and PocketIC reconciliation filters passed, while the later full PocketIC target remained non-terminal because of an unrelated HTTP-adapter failure.

## Related documents

- [stable-memory-inventory.md](../storage/stable-memory-inventory.md)
- [property-index.md](property-index.md)
- [../adr/0059-create-index-migration-backfill.md](../adr/0059-create-index-migration-backfill.md)
- [label-index.md](label-index.md)
- [../adr/0015-label-stats-projection-log.md](../adr/0015-label-stats-projection-log.md)
- [../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md)
- [../architecture/acid-roadmap.md](../architecture/acid-roadmap.md)
- [../sharding/standalone-mode.md](../sharding/standalone-mode.md)
- [../federation/query-semantics.md](../federation/query-semantics.md)
- [../architecture/refactoring-roadmap.md](../architecture/refactoring-roadmap.md)
