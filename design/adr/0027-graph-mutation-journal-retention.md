# 0027. Graph mutation journal retention

Date: 2026-06-21
Status: implemented
Last revised: 2026-07-22
Anchor timestamp: 2026-07-22 20:57:56 UTC +0000

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

- A `mutation_id` is reserved by the router against a `ClientMutationKey` record.
  The router re-sends `execute_plan(mutation_id)` to a shard whenever that record
  is within TTL and not yet `completed_row_count`. After all shards complete and
  project (or zero-shard completion), the router sets `completed_row_count` and
  **never contacts the graph for that id again** — but that terminal state lives
  on the *router*; the graph has no direct signal for it.
- The router record is GC'd at `created_at_ns + CLIENT_MUTATION_KEY_TTL_NS`
  (7 days, ADR 0025). After that the same client key allocates a **fresh**
  `mutation_id`; the old id is retired forever and can never be replayed.

### Why ack-through-seq is not a safe trigger

The graph's only inbound non-replay signal is `ack_label_stats_deltas_through`.
It is **unsound** as an eviction trigger for three independent reasons:

1. Mutations that emit no label-stats deltas produce **no ack at all**.
2. The acked value is a **shard-global projection high-water mark** advanced by
   *later* mutations — `ack_through >= emitted_delta_last_seq(M)` can hold purely
   because of some `M2 > M` and says nothing about `M`'s router completion.
3. The router acks (graph drops deltas) at the inter-canister `await` boundary
   **before** it durably records shard completion. A trap in that window leaves
   the record not `completed`, so the router still re-sends `execute_plan(M)` and
   depends on the journal dedup — after the ack already passed `M`'s seq.

So ack proves "deltas were projected," not "the router will never replay this id."

## Decision

Retain journal entries on a **time bound ≥ the router's replay TTL**, swept by an
amortized write-path GC. Time is the only graph-observable invariant that bounds
replay.

1. **Timestamp every entry.** `GraphMutationJournalEntry` gains
   `recorded_at_ns: Option<u64>`, stamped on both `Incomplete` and `Completed`
   writes (persisted `Incomplete` entries are also dedup markers, so they are
   age-bounded too — never leaked, never dropped within the replay window).
   `None` decodes from pre-ADR-0027 Candid bytes (the field is `#[serde(default)]`
   and omitted on legacy values), so the change is backward-compatible with no
   layout-version bump (region 39 stays `Canonical` / `RebuildPath::None`).

2. **Retention = `GRAPH_MUTATION_JOURNAL_RETENTION_NS` = 9 days.** A
   one-directional lower-bound coupling to the router's 7-day
   `CLIENT_MUTATION_KEY_TTL_NS` (TTL + margin for clock skew and the GC "one extra
   lap" slack). It is deliberately **not** an exact duplicate of the router
   constant: graph must not depend on router, and dedup safety only requires
   `graph retention >= router TTL`. The coupling is documented at the constant and
   here (single source of the *contract*, even though the value is restated).

3. **Amortized write-path GC** (ADR 0025 mechanism B, mirrored). Each
   completed-journal write — the per-mutation growth source — funds one bounded
   step that scans `MUTATION_JOURNAL_GC_BUDGET` (2) entries from a heap-only
   round-robin cursor and evicts those older than retention. The sweep is skipped
   while the journal length stays below a heap-only minimum threshold
   (`MUTATION_JOURNAL_GC_MIN_LEN`), avoiding the large fixed cost of a stable
   B-tree range cursor for the common case of a short journal; the cursor is
   ephemeral (`thread_local`): resetting to the start on upgrade just restarts the
   lap, and region 39 is the stable source of truth.

4. **Lazy-stamp legacy entries.** A swept entry with `recorded_at_ns == None` is
   stamped to `now` instead of evicted, so the pre-upgrade backlog ages out from
   *upgrade time* rather than being dropped immediately (which would risk evicting
   an in-flight entry written just before upgrade).

## Rejected alternatives

- **Evict on `ack_label_stats_deltas_through` reaching `emitted_delta_last_seq`.**
  Unsound for the three reasons above.
- **Evict any `Completed` entry immediately.** The router replays `execute_plan`
  against `Completed` entries — that *is* the dedup path.
- **Count/LRU cap.** A fixed entry count does not bound *time*; under variable
  traffic it can evict within the replay window or retain far past it.
- **Explicit router→graph "mutation retired" ack** on the `completed_row_count`
  transition. The only *exact* "router will never replay" signal, but it adds a
  cross-canister call on the completion hot path, modifies the router dispatch
  loop, and still needs a TTL backstop for records GC'd before the ack lands.
  Deferred as a possible future precision tightening; the time bound is sound and
  self-contained on the graph today.

## Consequences

- Region 39 is bounded by the replay TTL working set, like the router journal
  (region 7) under ADR 0025.
- Retention is coupled to the router TTL by contract; if `CLIENT_MUTATION_KEY_TTL_NS`
  ever grows past `GRAPH_MUTATION_JOURNAL_RETENTION_NS - margin`, the graph
  constant must grow too. Both ADR 0025 and this ADR call out the relationship.
- The amortized sweep is conditionally skipped while the journal is short, so
  the fixed per-write sweep cost is avoided in the common case without changing
  the long-term growth bound. Once the journal exceeds the heap-only threshold,
  the normal round-robin sweep resumes.
- No admin sweep endpoint is added; the amortized write-path step keeps pace with
  growth without a timer. An operator-driven paginated sweep (mirroring
  `admin_sweep_expired_client_mutation_keys`) remains a possible future addition
  if a one-shot drain is ever needed.

## References

- ADR 0015 — label stats projection log and graph mutation journal (introduces the journal).
- ADR 0024 — mutation journal completion vs deferred index flush (`Completed` semantics).
- ADR 0025 — client-mutation idempotency journal retention/compaction/GC (router twin; TTL source).
- `design/storage/stable-memory-inventory.md` — region 39 retention note.
