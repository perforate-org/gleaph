# 0027. Graph mutation journal retention

Date: 2026-06-21
Status: Partially Implemented — recoverable PlanExecution retention gap open
Last revised: 2026-09-22
Anchor timestamp: 2026-09-22 18:20:36 UTC +0000

## Verified retention limitation

Recoverable Router PlanExecution records can outlive Graph's age-only nine-day journal
window. Router's seven-day terminal receipt window does not bound unfinished canonical replay.
The Router count-capture and actual-send prerequisite below closes new sends after durable
capture; it does not retain a Graph outcome lost before capture. Exact receipt retention and
acknowledged scalar retirement remain open in
[GAP-2026-09-22-001](../implementation-gaps.md#gap-2026-09-22-001--recoverable-plan-execution-can-outlive-graph-journal-retention).

## Context

`GRAPH_MUTATION_JOURNAL` (stable region 39, `Canonical`) is the graph shard's
idempotent-replay dedup store: one entry per `mutation_id`, written `Incomplete`
during a DML and overwritten `Completed` at the end. On a router replay of the
same `mutation_id`, `run_wire_plans_inner` (`crates/graph/src/gql_run.rs`)
short-circuits on a `Completed` entry (returns the cached outcome instead of
re-applying) and rejects a re-applied-but-`Incomplete` entry. The journal was
the graph-side twin of the router's client-mutation journal, but — unlike the
router journal (ADR 0025) — it had **no retention**: `get`/`insert` only, one
entry per `mutation_id` forever. Over a canister's lifetime this grows unbounded.

The hard question is the **eviction trigger**. Evicting an entry that the router
can still replay re-applies the DML = double-application / corruption. Evicting
too late leaks. We traced the full router↔graph `mutation_id` lifecycle:

- For the implemented `GraphMutationRequestIdentityV1::PlanExecution` path, a
  `mutation_id` is reserved by the
  router against a `ClientMutationKey` record. The router re-sends
  `execute_plan(mutation_id)` while a retained record still needs canonical completion.
  Non-terminal records do not expire by age. After all shards complete and project (or
  zero-shard completion), the router sets `completed_row_count`; the actual-send fence
  permits **no new canonical dispatch** for that identity. This does not cancel earlier calls
  or prohibit projection work. That terminal state lives on the *router*; Graph has no direct
  scalar-retirement signal for it.
- Terminal Router records become age-eligible from
  `terminal_at_ns + CLIENT_MUTATION_KEY_TTL_NS` (7 days, ADR 0025), subject to
  ownership pins. Pending bulk update children retain their row evidence even then.
  After physical deletion a new reservation gets a fresh ID; this does not bound
  the age of an unfinished record or already dispatched request.

### Why ack-through-seq is not a safe trigger

The graph's only inbound non-replay signal is `ack_label_stats_deltas_through`.
It is **unsound** as an eviction trigger:

1. Mutations that emit no label-stats deltas produce **no ack at all**.
2. The acked value is a **shard-global projection high-water mark** advanced by
   *later* mutations — `ack_through >= emitted_delta_last_seq(M)` can hold purely
   because of some `M2 > M` and says nothing about `M`'s router completion.
3. Router now writes the captured count before projection may suspend, but a projection ack
   still does not identify an exact mutation retirement or establish that earlier requests are
   quiescent. It cannot replace the missing scalar lifetime/retirement handshake.

So ack proves "deltas were projected," not "the router will never replay this id."

## Decision

### Router terminal-anchor interaction (ADR 0057, 2026-08-02)

The Router's `terminal_at_ns` anchors terminal receipt recovery and GC, not unfinished canonical
replay. Graph PlanExecution still ages from its recorded timestamp; ordered entries age only after
explicit retirement. The existing nine-day window is unchanged, but comparing it with Router's
seven-day terminal receipt window does not prove that unconfirmed scalar outcomes are retained
long enough. The source-verified gap above supersedes that earlier safety inference.

The implemented PlanExecution mechanism is a nine-day age bound swept by amortized write-path GC.
Its recovery-safe lifetime predicate remains incomplete. ADR 0049's implemented ordered-family
predicate instead retains Active evidence without an age limit and starts GC age only at retirement.

1. **Timestamp every entry.** `GraphMutationJournalEntry` gains
   `recorded_at_ns: Option<u64>`, stamped on both `Incomplete` and `Completed`
   writes. Persisted `Incomplete` entries are also dedup markers; the same age-only
   mechanism applies to them, with the recovery-lifetime limitation above.
   The current fixed-length layout writes the timestamp slot on every persisted
   entry. Pre-0120 Candid bytes are intentionally discarded at the fresh-install
   boundary; no legacy decoder or in-place migration is supported.

2. **Retention = `GRAPH_MUTATION_JOURNAL_RETENTION_NS` = 9 days.** A
   one-directional lower-bound coupling to the router's 7-day
   `CLIENT_MUTATION_KEY_TTL_NS` (TTL + margin for clock skew and the GC "one extra
   lap" slack). It is deliberately **not** an exact duplicate of the router
   constant: Graph must not depend on Router. The original inequality
   `graph retention >= router TTL` is insufficient for non-terminal retention and
   pending-child pins; see the source-verified gap above.

3. **Amortized write-path GC** (ADR 0025 mechanism B, mirrored). Each
   completed-journal write — the per-mutation growth source — funds one bounded
   step that scans `MUTATION_JOURNAL_GC_BUDGET` (2) entries from a heap-only
   round-robin cursor and evicts entries that satisfy the request-kind-specific
   retention predicate. The sweep is skipped while the journal length stays
   below a heap-only minimum threshold
   (`MUTATION_JOURNAL_GC_MIN_LEN`), avoiding the large fixed cost of a stable
   B-tree range cursor for the common case of a short journal; the cursor is
   ephemeral (`thread_local`): resetting to the start on upgrade just restarts the
   lap, and region 39 is the stable source of truth.

4. **Lazy-stamp legacy entries.** A swept entry with `recorded_at_ns == None` is
   stamped to `now` instead of evicted, so the pre-upgrade backlog ages out from
   *upgrade time* rather than being dropped immediately (which would risk evicting
   an in-flight entry written just before upgrade).

5. **Ordered-batch override (implemented, ADR 0049).** The journal V1 has stable
   `NotApplicable | Active | Retired { at_ns }` retirement state.
   Journal reads expose only the derived `NotApplicable | Active | Retired`
   retirement enum; the timestamp stays Graph-internal.
   `GraphMutationRequestIdentityV1::PlanExecution` requires `NotApplicable` and
   retains the implemented age-only predicate above.
   `OrderedEdgeBatch`, `OrderedVertexBatch`, and `OrderedMixedBatch` require `Active`
   or `Retired` and are never evictable from `recorded_at_ns`: Router non-terminal replay has
   no TTL, so an active ordered entry is retained regardless of age. After
   required projections
   converge, Router invokes the authenticated, fingerprint-bound, idempotent
   family-specific retirement transition (`retire_ordered_mutation` for edge inserts).
   Only an ordered entry in `Retired { at_ns: t }` with
   `now > checked_add(t, GRAPH_MUTATION_JOURNAL_RETENTION_NS)` is eligible for
   the existing bounded amortized GC. Overflow or a future timestamp fails
   closed rather than saturating into eligibility. Write-path GC and any future
   operator GC share this request-kind-aware predicate and have no
   force-eviction bypass.

### Router capture and send prerequisite (implemented, 2026-09-22)

[ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md#scalar-count-capture-and-actual-sends)
owns the implemented monotonic per-shard count capture and fresh individual/chunk send checks.
A durably captured count closes canonical resend for that shard, even if projection later fails.
This is not an exact retained receipt: scalar compaction still discards targets, Graph still uses
`NotApplicable`, and no scalar retirement acknowledgement exists. An outcome lost **before durable
Router capture** can still age out on Graph and be re-executed by a fresh request.

Router's native paused-target contracts and owner-cost fixtures accompany the send boundary.
The three preparation writers also require the original reservation ID under
[ADR 0029's preparation boundary](0029-shard-local-atomicity-and-cross-canister-consistency.md#scalar-preparation-callback-identity).
That protects a newer same-key Router reservation, not Graph evidence. A fresh gate costs about
4.89 M instructions with a 1 KiB request fingerprint and 125.74 M with 1 MiB: it decodes the full
retained record. The safety check remains necessary, but these measurements do not approve
production cost/admission bounds. End-to-end retention repair remains open.

## Rejected alternatives

- **Evict on `ack_label_stats_deltas_through` reaching `emitted_delta_last_seq`.**
  Unsound for the reasons above.
- **Evict any `Completed` entry immediately.** The router replays `execute_plan`
  against `Completed` entries — that *is* the dedup path.
- **Count/LRU cap.** A fixed entry count does not bound *time*; under variable
  traffic it can evict within the replay window or retain far past it.
- **Explicit router→graph retirement for every request kind.** The original rejection
  relied on a bounded PlanExecution replay TTL. That rationale no longer covers
  recoverable scalar records. Existing ordered families use exact retirement;
  the separate scalar gap must choose its enforcing lifetime rule before any
  broader extension is accepted. No new universal retirement subsystem is selected.

## Consequences

- PlanExecution entries have an age-bounded Graph working set, but this is not
  a recovery-safety guarantee for longer-lived Router records. Active ordered entries
  additionally represent Router-owned replay/retirement obligations without an age
  bound; journal-only reconciliation and retirement recovery are needed for convergence.
- Increasing `GRAPH_MUTATION_JOURNAL_RETENTION_NS` alongside a Router TTL increase
  cannot by itself cover indefinitely recoverable PlanExecution records. Ordered
  retention starts only from its explicit retirement transition; fixing scalar
  retention requires an enforcing rule, not only larger constants.
- The amortized sweep is conditionally skipped while the journal is short, so
  the fixed per-write sweep cost is avoided in the common case without changing
  the long-term growth bound. Once the journal exceeds the heap-only threshold,
  the normal round-robin sweep resumes.
- No admin sweep endpoint is added; the amortized write-path step keeps pace with
  growth without a timer. An operator-driven paginated sweep (mirroring
  `admin_sweep_expired_client_mutation_keys`) remains a possible future addition
  if a one-shot drain is ever needed.
- A lost ordered retirement callback cannot re-enable canonical execution.
  Router persists `RetirementPending` before the call and retries only the
  idempotent retirement transition. If an already retired entry ages out before
  Router observes the callback, absence leaves the saga pending operator repair;
  it is not retirement proof and never authorizes canonical execution.

## References

- ADR 0015 — label stats projection log and graph mutation journal (introduces the journal).
- ADR 0024 — mutation journal completion vs deferred index flush (`Completed` semantics).
- ADR 0025 — client-mutation idempotency journal retention/compaction/GC (router twin; TTL source).
- `design/storage/stable-memory-inventory.md` — region 39 retention note.

---

## Addendum: cost attribution (Plan 0119, 2026-07-22)

Non-invasive encode probes show that the journal entry encode cost
(`journal_entry_encode`) dominates the commit path:

- ~182 K instructions to encode a 150-byte scalar completed entry.
- `journal_map_insert` total ~187 K, leaving only ~5–8 K for fresh-key
  `StableBTreeMap` I/O.
- Overwriting an existing journal key is much more expensive (~257 K residual),
  but canonical writes use fresh `mutation_id`s.

## Addendum: fixed-length manual layout (Plan 0120, 2026-07-23; revised 2026-07-29)

`GraphMutationJournalEntry` now uses a versioned, fixed-length primary record
plus an optional appendix instead of Candid encoding:

- Primary: version (1), `mutation_id` (8), `state` (1), `row_count` (8), validity
  bitmap (1), fixed slots for `emitted_delta_first_seq` (8), `emitted_delta_last_seq`
  (8), `recorded_at_ns` (8), and `next_index` (4), appendix flags (1), appendix
  length (4). The bitmap marks which optional slots are live.
- Appendix: `hot_forward_vertices` (`count` + `count * u32`) and/or bulk progress
  (`operation_count`, `completed_count`, `row_count_len`, `row_count_len * u64`).
- The appendix remains length-delimited and accepts any `u32`-representable
  hot-forward or bulk-progress count that fits the shared
  `MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES` limit; decode applies the
  same byte boundary.
- `Storable::BOUND` remains `Unbounded`. A `Bounded` experiment with a large
  `max_size` regressed `StableBTreeMap` fresh-key inserts, so bounds enforcement
  logical payload enforcement stays at encode and decode time.

Measured impact vs. the Plan 0119 Candid baseline:

- `journal_entry_encode`: ~182 K → ~0.6 K instructions.
- `journal_map_insert`: ~187 K → ~7–9 K instructions (fresh key).
- `canonical_segment_insert_vertex` total: ~65% reduction.
- `canonical_segment_insert_edge` total: ~43% reduction.
- `canonical_segment_insert_bundle_4` total: ~53% reduction.
- `canonical_segment_insert_bundle_16` total: ~23% reduction (~278 K absolute).

No retention window, GC design, or replay semantics changed for the implemented
`GraphMutationRequestIdentityV1::PlanExecution` path. ADR 0049's implemented
ordered request-kind override and the separate scalar lifetime gap are specified above.
