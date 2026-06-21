# 0025. Client-mutation idempotency journal retention, compaction, and GC

Date: 2026-06-20
Status: implemented (eviction predicate and timer stance revised by ADR 0029 Phase 4)
Last revised: 2026-06-21
Anchor timestamp: 2026-06-21 13:18:04 UTC +0000

## Context

The router deduplicates client mutations through `ROUTER_MUTATION_BY_CLIENT_KEY`
(stable region 7), a `BTreeMap<ClientMutationKey, RouterMutationRecord>` keyed by
`(caller, graph_id, client_key)`. A record carries `created_at_ns`, the resolved
label/property tables, and a `Vec<RouterMutationShard>` — it is not trivial in size.

`CLIENT_MUTATION_KEY_TTL_NS` (7 days) defines the idempotency window, but it was only
ever consulted **lazily**, when the *same* key was presented again
(`reserve_mutation_id_for_client_key_at`). Under normal traffic each mutation uses a
fresh client key, so every mutation inserted a record that was never reclaimed:

- `ROUTER_MUTATION_BY_CLIENT_KEY` grew unboundedly in steady state (no upgrade or
  partial failure required),
- eventually exhausting stable memory and degrading/bricking the router.

There was no eviction, sweep, or bound anywhere; only `clear_new()` at full re-init.

## Decision

Keep `created_at_ns` on the record as the single source of truth for age, and bound the
journal in **size** and **count** without adding any stable region or timer, via three
complementary mechanisms:

### E — compact completed records (bound per-record size)

Once a mutation is fully done — every shard `completed` **and** `projection_advanced`, or
a zero-shard completion — the resolved label/property tables and the `Vec<RouterMutationShard>`
fan-out are never read again: replay short-circuits on `completed_row_count`
(`run_gql_dml` returns at the `router_mutation_completed_row_count` check before touching
the heavy fields). At that point `record_router_mutation_shard_projection_advanced` pins
the final `completed_row_count` and drops `resolved_labels`, `resolved_properties`, and
`shards`. The record shrinks to `{ mutation_id, created_at_ns, request_fingerprint,
completed_row_count }`, which is all replay and TTL eviction need.

### B — amortized GC on the write path (bound count, automatic)

`reserve_mutation_id_for_client_key_at` is the only source of growth (a new client key
inserts a record). After each new insert it runs `gc_expired_client_mutation_keys`, which
advances a **heap-only round-robin cursor** over the journal keyspace, examines
`MUTATION_GC_BUDGET` (2) records, and removes those past `CLIENT_MUTATION_KEY_TTL_NS` that
are in a **terminal** lifecycle phase (`Completed` or `Failed`; see the eviction-predicate
revision below). Each insert evicts up to 2 expired records, so eviction
keeps pace with insertion and the journal converges to its TTL working set with no
operator action, no timer, and no second stable index. The cursor is ephemeral: on upgrade
it resets to the start, which merely restarts the lap (the journal itself is fully stable).

### Backstop — operator sweep (bulk / forced)

`RouterStore::admin_sweep_expired_client_mutation_keys(caller, start_after, max_scan)`,
exposed as the `#[update] admin_sweep_expired_client_mutation_keys` endpoint
(`Role::Admin`), is the paginated bulk equivalent of B: it scans `max_scan` entries from
`start_after` and the operator drives it with `next_cursor` until `done`, exactly like the
backfill / projection steps. It exists to drain a large pre-existing backlog quickly or to
force a full pass; B alone keeps steady-state growth bounded. Both share one eviction core
(`evict_expired_client_mutation_keys`).

Only **terminal** records are ever removed (see the eviction-predicate revision below), so
no in-flight or recoverable saga is yanked even if it is wall-clock-expired.

### Eviction predicate revision (ADR 0029 Phase 4)

The original predicate evicted any record that was past the TTL and **not**
`routing_in_progress`. That stranded a federated mutation whose canonical writes were
durable but whose derived projections had not yet converged (`CanonicalCommitted` /
`ProjectionPending`) and whose canonical fan-out was only partly applied
(`CanonicalPending`): such records have `routing_in_progress == false`, so the old rule
made them TTL-evictable, silently discarding a saga the recovery driver still had to finish.

ADR 0029 Phase 4 makes the shared eviction core (`evict_expired_client_mutation_keys`)
evictable **iff the record is terminal** — `RouterMutationRecord::is_terminal()`, i.e. the
lifecycle phase is `Completed` or `Failed`. Non-terminal sagas (`Routing`,
`CanonicalPending`, `CanonicalCommitted`, `ProjectionPending`) are retained as recovery
targets regardless of age. This subsumes the old `routing_in_progress` exclusion (a routing
record is non-terminal) and additionally protects committed-but-unprojected sagas. Age is
still tracked solely by `created_at_ns`; terminal records past the TTL evict exactly as
before.

### Alternatives considered

- **Time-ordered secondary index (new stable region) + amortized sweep.** Gives precise
  oldest-first eviction in O(expired). Rejected: it adds a stable region (layout-registry +
  inventory + migration surface) and duplicates `created_at_ns` ordering. B achieves
  automatic bounding without it, trading exact oldest-first for a round-robin lap (an entry
  may outlive its TTL by at most one lap before being visited — still bounded). The index
  can be added later behind the same TTL contract without changing observable semantics.
- **Operator-only sweep (no B).** Bounded only if the operator runs the loop; fragile.
  Kept as the backstop, not the primary mechanism.
- **Router timer.** This ADR originally kept the router free of `ic_cdk_timers`/`heartbeat`;
  all retention maintenance is operator- or write-path-driven, and B fits that model.
  **Superseded for recovery by ADR 0029 Phase 4**, which introduces a single, bounded,
  self-rescheduling recovery timer for autonomous federated-saga convergence. Retention GC
  (B + sweep) remains write-path/operator-driven as described here; the recovery timer is a
  separate concern (liveness of unfinished sagas, not journal size) and does not evict.
- **Per-write full scan.** O(n) per mutation; unacceptable.

## Consequences

- Steady-state growth is bounded **automatically**: B caps the record count to the TTL
  working set on the write path, and E caps each record's size. No operator action or
  upgrade hook is required for the steady state; the operator sweep remains for bulk drain.
- Minor semantics change: a key older than the TTL that has already been evicted is treated
  as new on re-presentation (a fresh mutation id) instead of returning
  `client_mutation_key expired`. This only affects keys reused beyond 7 days, far past any
  reasonable retry window; the rejection still applies to expired-but-unevicted keys.
- `created_at_ns` remains the only source of truth for record age; B/E/sweep add no
  duplicated stable state (the GC cursor is ephemeral heap).
- If mutation traffic stops, B stops too (no growth either); the operator sweep can still
  reclaim the final aged-out working set on demand.

## Tests

`crates/router/src/facade/store/tests.rs`:
`router_mutation_journal_tracks_shard_completion` (extended: asserts E compaction),
`amortized_gc_evicts_expired_and_keeps_fresh` (B),
`sweep_removes_expired_but_keeps_fresh_and_in_progress`,
`sweep_paginates_with_cursor_until_done`,
`sweep_requires_admin_and_nonzero_budget`,
`ttl_eviction_retains_nonterminal_saga_but_evicts_terminal` (ADR 0029 Phase 4 predicate).
