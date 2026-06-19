# 0021. Resumable super-node DETACH DELETE (deferred incident-edge purge)

Date: 2026-06-19
Status: proposed (revised 2026-06-19 after a read-gate breadth finding; see
"Implementation finding" and the staged Migration)
Last revised: 2026-06-19

Builds on [ADR 0020](0020-deferred-maintenance-timer-drain.md) (timer-driven
deferred maintenance) and refines the incident-edge invariant of
[ADR 0017](0017-graph-vertex-existence-ssot.md).

## Context

`DETACH DELETE v` removes a vertex together with all its incident edges. On a
graph shard this lands in `GraphStore::commit_detach_delete_vertex`
(`crates/graph/src/facade/store/vertex_delete.rs`) →
`DeferredBidirectionalLaraGraph::delete_vertex_deferred`
(`crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs:2018`).

`delete_vertex_deferred` runs a **synchronous** loop:

```text
while has_incident_edges(vid):
    remove one forward out-edge of v   → also removes the reverse half at the neighbor
    or remove one reverse in-edge of v → also removes the forward half at the neighbor
clear v's label buckets (both orientations)
set v's row = tombstone        # LAST
```

Edge removal is already logical+cheap (slab tombstone, physical compaction
deferred to the ADR-0020 queue), but the loop runs **O(degree)** iterations and
each iteration also scans and tombstones the **neighbor's back-edge bucket**
(`remove_directed_deferred` / `remove_undirected_deferred` call
`remove_edge_matching` on the neighbor). The graph facade additionally collects
**every** incident edge handle into a `Vec` and clears each edge's sidecars
(`collect_incident_edge_handles_for_delete` + `commit_clear_edge_sidecars`),
also O(degree).

Why neighbors must be touched today: **no read path checks the neighbor vertex's
tombstone bit.** Edge iteration in `crates/ic-stable-lara/src/labeled/graph/traverse.rs`
filters only the *edge slot's own* tombstone; expand
(`crates/graph/src/plan/query/executor/expand/`) maps `edge.neighbor_vid()`
straight to a destination with no liveness check. Correctness rests entirely on
the invariant **"a tombstoned vertex has no surviving incident edges"**, codified
in ADR 0017 ("delete DML must enqueue removals before tombstoning the CSR row").
The synchronous neighbor-back-edge removal is what upholds that invariant.

## Problem

For a **super-node** (high in/out degree), `DETACH DELETE v` performs O(degree)
synchronous work — neighbor bucket scans, edge tombstoning, and per-edge sidecar
clears — in a single update message. A large enough degree exceeds the
per-message instruction (40 B) / 2 GiB stable read-write ceiling and **traps**,
rolling back the whole statement. The vertex then **cannot be deleted at all**
through `DETACH DELETE`: every attempt re-traps. This is a liveness vulnerability
at the storage/API boundary, analogous to the unbounded shard detach and the
unbounded maintenance drain already fixed, but it differs in one critical way:

> Unlike physical reclamation (ADR 0020), incident-edge removal is **logical** —
> it changes query visibility. It cannot simply be deferred, because until the
> neighbors' back-edges are gone, reads from those neighbors still yield edges to
> the (tombstoned) vertex.

So a resumable delete must keep `v` and all its incident edges **invisible from
the moment the statement commits**, while the physical purge proceeds across
later messages.

## Existing Architecture Assessment

What already exists and can be reused:

- **Deferred maintenance queue + adaptive timer (ADR 0020).** A stable,
  budgeted, self-draining work queue with event-driven re-arm. A vertex purge is
  a natural new work item on this queue — no new execution trigger is needed.
- **Vertex tombstone bit** (`LabeledVertex::is_tombstone`) already marks `v`
  itself invisible to node scans (`scan/streaming.rs`, `scan/index.rs`).
- **Edge slot tombstones** already make individual edges invisible without
  physical compaction.
- **A working phased-delete reference design** exists in the *non-labeled*
  `DeferredBidirectionalLaraGraph`
  (`crates/ic-stable-lara/src/bidirectional/deferred.rs`):
  `MaintenanceWorkItem::DeleteVertex { vid, phase, cursor, removed_edges }` with
  phases `RemoveOutgoing → ClearForwardRow → RemoveIncoming → ClearReverseRow`,
  a `max_delete_edge_steps` budget, and `DeleteEdgeObserver` callbacks. It
  tombstones both rows **first**, then purges incrementally.

What is missing for the **labeled** path the graph crate actually uses:

1. The labeled `MaintenanceWorkItem` enum
   (`labeled/bidirectional/deferred.rs:68`) has only `Compact*` variants — **no
   delete/purge work item**.
2. **No read-time visibility gate for dangling back-edges.** Because today's
   delete is synchronous, no reader checks neighbor liveness. If we defer the
   neighbor purge, reads *will* observe edges to a tombstoned vertex unless a
   filter is added.
3. **No "pending purge" vertex set.** There is no fast way for a reader to know a
   vertex is mid-delete.

Conclusion: the execution machinery (queue + timer) and the reference algorithm
both exist; the gap is (a) a labeled purge work item, (b) a read-time visibility
gate, and (c) a small amount of state to drive that gate. This is an **extension
of the existing deferred-maintenance domain**, not a new subsystem.

## Alternatives

### A. Minimum change — bounded check + clean error (no resumability)
Before deleting, if the degree exceeds a safe budget, return a deterministic
error instead of trapping; the client must shrink the vertex (delete edges in
batches) before deleting it.

- Benefit: tiny, fully invariant-preserving, **zero read-path cost**, no new
  state. Converts an unrecoverable trap into a recoverable error.
- Drawback: single-statement super-node `DETACH DELETE` is **unsupported**;
  pushes O(degree) chunking onto every client.
- Verdict: viable safety floor, but does not deliver the requested capability.

### B. Router-driven resumable loop (mirror admin shard detach)
The router calls a graph endpoint repeatedly with a cursor until the vertex is
fully deleted, each call doing a bounded chunk.

- Benefit: reuses the bounded-step + cursor pattern from shard detach.
- Drawback: `DETACH DELETE` is a single GQL statement executed inside one
  `execute_plan_update` message, not a router admin op; splitting one statement's
  commit across messages breaks update-call atomicity and still needs read-time
  filtering for the in-flight window. Higher complexity than C for no visibility
  benefit.
- Verdict: rejected — wrong boundary (statement execution is not a router loop)
  and does not avoid the filtering requirement.

### C. Tombstone-first + deferred phased purge on the maintenance timer (chosen)
Tombstone `v` and record it in a **pending-purge set** synchronously, enqueue a
phased `DeleteVertex` work item on the ADR-0020 queue, run one bounded inline
pass, and arm the timer for the rest. Reads gain a **gated** neighbor-liveness
filter that is active only while the pending-purge set is non-empty.

- Benefit: reuses the queue/timer/observer machinery and the non-labeled
  reference algorithm; small deletes still finish in the first bounded pass
  (identical to today, no pending entry, no read cost); only true super-nodes
  spill to the timer; one read-filter chokepoint in LARA traverse covers all
  consumers.
- Drawback: adds a stable pending-purge set; introduces a read-time filter
  (gated, but a per-edge neighbor check during purge windows); weakens the ADR
  0017 "tombstoned ⇒ no incident edges" invariant into "tombstoned ⇒ no
  *visible* incident edges", requiring a documented read-time gate.
- Verdict: chosen — delivers resumable super-node delete by extending existing
  domains, with steady-state read cost held near zero by the pending-set gate.

## Decision

Implement **Alternative C**. Keep Alternative A's bounded check as the **safety
floor** underneath it (a purge that somehow cannot make progress must error, not
trap).

### 1. Delete commit (graph facade + labeled LARA)

`delete_vertex_deferred` (labeled) becomes:

1. Clear `v`'s **own** property/label sidecars (bounded; these are per-vertex,
   not per-degree).
2. Set `v`'s row tombstone and clear its label buckets **first** (so `v` is
   immediately invisible as a vertex and as an edge source).
3. Insert `v` into the **pending-purge set** and enqueue
   `MaintenanceWorkItem::DeleteVertex { vid, phase: RemoveOutgoing, cursor: 0,
   removed_edges: 0 }`.
4. Run one **bounded** inline maintenance pass (delete budget) and **arm the
   timer** (ADR 0020). A small vertex completes here and leaves the pending set
   before the call returns — preserving today's immediate-consistency behavior
   for the common case. A super-node spills to the timer.

Per-edge **sidecar clearing** moves into the purge step via a delete-edge
observer (mirroring the non-labeled `DeleteEdgeObserver`), so the graph facade no
longer collects an O(degree) `Vec` up front.

### 2. Source-tombstone read gate (not required — analysis)

`v`'s **own** out-edges do not need an extra source gate: a tombstoned `v` can
never be *bound* as a traversal element (node scans already gate on the start
tombstone; `resolve_local_vertex` rejects tombstoned ids; and the neighbor gate
in §3 filters every edge whose destination is `v`, so `v` is never reached as an
expand destination either). With `v` never bound, hop-`n+1` never expands from
`v`, so `v`'s out-edges are never yielded to a reader. Strengthening the
`lara/edge/scan.rs` early-return would additionally **break the purge itself**,
which must iterate the (tombstoned) `v`'s buckets to find neighbor back-edges.
The neighbor gate (§3) is therefore the single necessary visibility mechanism.

### 3. Neighbor read gate (new, gated)

When iterating edges, skip any edge whose **neighbor** (destination) vertex is in
the pending-purge set, **but only while that set is non-empty** (steady state
short-circuits on `is_empty()` and pays nothing). The filter must run at
**edge-yield time**, not only at destination binding — see the Implementation
finding below for why binding-time filtering is insufficient and where the gate
must live.

### 4. Pending-purge set (new stable state)

A stable `StableRoaringBitmap` of local `VertexId`s that are tombstoned but not
yet fully purged: insert on delete commit, remove when the `DeleteVertex` job
finishes. It drives the read gate and survives upgrades (an in-flight purge must
resume after upgrade — the ADR-0020 timer is re-armed in `post_upgrade`, and the
queue + bitmap are stable).

**Ownership (revised):** because the read gate lives in the **graph facade**
(see the Implementation finding), the bitmap is owned by the **graph crate** (new
`MemoryId`, empty on existing canisters), not by `ic-stable-lara`. This avoids
threading a new memory through LARA's 30-argument graph constructor. Completion
is propagated from LARA to the graph via a `on_vertex_purge_completed(vid)`
callback on the delete-edge observer, which clears the bitmap. Insert happens on
the graph delete path. The queue (stable, in LARA) holds the *work*; the
graph-side bitmap is the membership index for reads, updated on the same logical
delete/complete path.

### 5. Labeled purge work item

Add `MaintenanceWorkItem::DeleteVertex { vid, removed_edges }` to the labeled
enum (16-byte fixed encoding: tag + `vid` + `removed_edges`). The labeled path
needs **no positional cursor or explicit phases** (unlike the non-labeled
reference): each step removes the *current first* incident edge (forward out
first, else reverse in), which also tombstones the neighbor's counterpart, so the
"first" advances naturally as the set shrinks. One edge is processed per queue
pop (matching the existing `Compact*` one-step pattern), so the existing
instruction / work-item budget bounds the work; no `max_delete_edge_steps`
handling is added to the labeled loop. When no incident edges remain, the step
clears `v`'s label buckets, writes the empty tombstone row, fires
`on_vertex_purge_completed(vid)`, and completes.

Because the dedup `dirty` bitmap key is a lossy hash (acceptable for compaction,
not for a correctness-critical delete), `DeleteVertex` items bypass the dirty
gate: `pop_next` treats a popped `DeleteVertex` as always valid, and idempotency
is guaranteed by the vertex tombstone (a vertex cannot be deleted twice).

## Consequences

### Positive
- Super-node `DETACH DELETE` no longer traps; it always makes bounded progress
  and completes across timer ticks.
- Reuses ADR-0020 queue/timer and the proven non-labeled phased-delete algorithm;
  no new execution trigger or subsystem.
- Small deletes are unchanged (finish in the first bounded pass; no pending entry,
  no read-filter cost, immediate consistency).
- Visibility stays correct for all query operators via the gated edge-yield
  filter at the graph facade edge-read entry points (see Implementation finding).

### Trade-offs
- **New stable state** (pending-purge bitmap) and a **new stable work-item
  variant** — additive, no repack, but a stable-format addition that must be
  initialized on existing canisters (empty set / no in-flight jobs).
- **Read-path gate breadth** (revised): the gate is **not** a single chokepoint;
  it spans the graph facade's edge-read entry points and must apply at edge-yield
  (not only destination binding). It is gated on `pending_purge.is_empty()` so
  steady-state cost is ~one branch, but the *implementation surface and test
  matrix are wide* — this is the dominant cost/risk of the change.
- **Invariant refinement**: ADR 0017's "tombstoned ⇒ no incident edges" becomes
  "tombstoned ⇒ no *visible* incident edges"; the read-time gate is now part of
  the existence SSOT. ADR 0017 must be updated.
- Reclamation is eventually-consistent for super-nodes: between commit and the
  final tick, dangling back-edges physically exist (but are filtered).

## Implementation finding (2026-06-19): read-gate breadth

Investigation during implementation showed the read gate is **wider and riskier**
than the "single LARA traverse chokepoint" this ADR first assumed:

- **No single LARA yield point.** `labeled/graph/traverse.rs` (~3250 lines)
  yields edges from ~10 sites across dense-prefix, hybrid/overflow-replay, point
  lookup, and descending paths, *including batched payload-first paths* that do
  not pass through a per-edge `next()`. Filtering inside LARA core would touch all
  of them and the batch builders — high regression risk in the storage core.
- **~12 graph-facade entry points.** `facade/store/edge_scan.rs` exposes
  `for_each_directed_out_edges`, `…_in_edges`, `…_undirected` (each with
  `_for_label` and `_unchecked` variants), plus Vec and iterator forms.
- **Binding-time filtering is insufficient.** Filtering only when an expand
  destination is bound/projected misses **anonymous-target patterns**
  (`(n)-[e]->()`), which bind the edge without projecting the destination; a
  dangling back-edge to a pending-delete vertex would remain observable. Full
  correctness therefore requires filtering at **edge yield**.

Conclusion: the gate belongs at the **graph facade edge-read entry points** (the
layer that already owns ADR-0017 liveness via `is_vertex_live`), applied at
edge-yield, gated on the pending-purge set. This is the correctness-critical,
test-heavy part of the change and is sequenced last.

### Consolidation: closure-wrap vs iterator-direct (2026-06-19)

The scattered yield sites can be reduced to **one policy predicate + a small set
of thin per-visit-shape wrappers**, because every facade read funnels a closure
into LARA:

- **Verdict: closure-wrap at the facade, not iterator-direct.** Query execution
  uses the `for_each_*` closure family exclusively (`for_each_csr_expand_edge` for
  expand; `path.rs` for path finding; payload-batch visitors) — there is **no
  `_edges_iter` usage in the executor**. Switching to iterator-direct filtering
  would require rewriting expand/path execution **and** would lose the for_each
  family's payload-batching / scratch-reuse optimizations (`_with_payloads`,
  `_with_payload_slices_reusing`, edge/value batch paths), making
  property-projecting traversals slower. The wrapper adds only ~1 inlined,
  gated branch per edge in steady state (off when `has_pending_vertex_purges()`).
- **One predicate, few wrappers.** Visit shapes that carry the edge —
  `FnMut(Edge)`, `FnMut(&Edge, &[u8])`, `FnMut(LabeledEdgePayloadBatch<Edge>)` —
  all expose `edge.neighbor_vid()` (always the counterpart relative to the queried
  vertex), so one direction-agnostic predicate
  `edge_hidden_by_purge(counterpart)` serves all of them.
- **Value-only batches need a fallback.** `FnMut(LabeledPayloadValueBatch)`
  yields property values without edge identity, so it cannot filter by neighbor.
  While `has_pending_vertex_purges()` is true, bypass this fast path in favor of
  the edge-bearing batch path (purges are rare; this is an acceptable, localized
  fallback).
- **`_unchecked` ≠ "do not filter".** The suffix means "skip vertex-range
  validation"; some `_topology_unchecked` reads in `path.rs` are query-visible and
  **must** filter. The real discriminator is "is this a query-visible read?" The
  purge itself does **not** go through the facade (it uses LARA-internal
  `asc_out_edges`), so it is naturally exempt without special-casing suffixes.

## Migration (staged)

Sequenced so each stage is independently committable and the trap is removed
early, deferring the wide read-gate to last:

### Stage 0 — Safety floor (eliminates the trap; invariant-preserving) — implemented 2026-06-19
- In `commit_detach_delete_vertex`, bound the synchronous incident-edge work; if a
  vertex's incident degree exceeds a safe budget, return a deterministic
  `GraphStoreError::VertexDeleteTooLarge` instead of risking a trap. Converts an
  unrecoverable trap into a recoverable, testable error. (Plain
  `commit_delete_detached_vertex` already requires zero incident edges via
  `VertexNotDetached`, so only the detach path needs the bound.)
- Implemented as:
  - LARA `LabeledLaraGraph::vertex_live_edge_count` (sums bucket degrees;
    O(#labels), since the vertex row's `degree()` is a bucket count in labeled
    mode) and `DeferredBidirectionalLabeledLaraGraph::incident_degree`
    (forward + reverse).
  - `GRAPH_MAX_SYNC_DETACH_DELETE_DEGREE` (provisional, conservative — 250_000) in
    `facade::ic_budget`.
  - `commit_detach_delete_vertex_bounded(vid, max_incident_degree)` performs the
    pre-mutation check; the public path passes the production constant.
  - `GraphStoreError::VertexDeleteTooLarge { vertex_id, incident_degree, limit }`,
    propagating through `PlanMutationError::Store` like `VertexNotDetached`.
- Tests: LARA `incident_degree_counts_forward_and_reverse`; graph
  `detach_delete_over_sync_limit_errors_without_mutation` (over-limit errors with
  no mutation; at-ceiling succeeds).
- No new stable state, no read-path change, no invariant change. The limit is
  provisional pending a Stage 1 delete benchmark and is removed entirely in
  Stage 2.

### Stage 1 — Resumable purge machinery (no visibility change yet) — implemented 2026-06-19
- Added labeled `MaintenanceWorkItem::DeleteVertex { vid, removed_edges }` packed
  into the existing fixed 16-byte work-item format (no stable-format migration;
  `removed_edges` is a `u32`), with `from_bytes`/`to_bytes` round-trip test.
- `pop_next` and `complete` bypass the dirty gate for `DeleteVertex`: the labeled
  `work_item_key` ranges share high bits with compaction keys, so a colliding
  compaction `complete` could otherwise clear a delete's dirty bit and drop it
  mid-job. Delete jobs are enqueued directly (`enqueue_delete_vertex`) and never
  deduped via the dirty bitmap.
- Added `DeleteEdgeObserver` (`on_delete_outgoing_edge`, `on_delete_incoming_edge`,
  `on_vertex_purge_completed`) + `NoopDeleteEdgeObserver`. The maintenance loop was
  refactored into `maintenance_with_observers(budget, slot_move_observer,
  delete_observer)`; `maintenance`, `maintenance_with_edge_slot_move_observer`, and
  a new `maintenance_with_delete_observer` delegate with Noops.
- One-edge-per-step processing arm (`process_delete_vertex_step`) removes a single
  incident edge per step via the existing `remove_undirected_deferred` /
  `remove_directed_deferred` primitives, then `finalize_vertex_delete` (factored out
  of `delete_vertex_deferred`, the single source of truth for row clear +
  tombstone). The loop honors `MaintenanceBudget::max_delete_edge_steps` and reports
  `processed_delete_edge_steps` / `completed_vertex_deletes`. `enqueue_vertex_delete`
  is the public entry point.
- `maintenance` now requires `E: PartialEq` (already satisfied — the facade already
  calls `delete_vertex_deferred`/`remove_*_deferred`, which require it).
- Tests: `delete_vertex_job_purges_incident_edges_phased` (one removal per incident
  edge, neighbors left with no dangling counterparts, vertex tombstoned),
  `delete_vertex_job_is_idempotent`, `delete_vertex_work_item_round_trips`.
- Still gated behind the Stage-0 budget: the production delete path stays
  synchronous (`delete_vertex_deferred`); the new job is exercised only by tests
  until Stage 2 wires it into the delete path.

### Stage 2 — Tombstone-first + read gate (enables resumable visibility)
- Add the graph-crate stable pending-purge `StableRoaringBitmap` (new `MemoryId`,
  empty on existing canisters) + `has_pending_vertex_purges()` / membership
  accessors; init in `init_graph`.
- Rewrite labeled `delete_vertex_deferred` to tombstone-first (per orientation,
  preserving buckets) + enqueue `DeleteVertex`; graph delete inserts into the
  pending set, runs one bounded inline pass, arms the ADR-0020 timer; move
  per-edge sidecar clearing into the graph-side delete-edge observer, which also
  clears the pending set on `on_vertex_purge_completed`.
- Add the gated edge-yield neighbor filter as **one predicate
  (`edge_hidden_by_purge`) + thin per-visit-shape closure wrappers** applied to
  the query-visible facade reads (`FnMut(Edge)`, `FnMut(&Edge, &[u8])`,
  `FnMut(LabeledEdgePayloadBatch<Edge>)`); during a purge, bypass the value-only
  batch fast path. Filter by "query-visible read", not by the `_unchecked`
  suffix; the purge bypasses the facade and is exempt.
- **Visibility test matrix** across expand (out/in/undirected), anonymous-target
  patterns, WCOJ, edge-property reads, label-scoped reads, and payload-first
  batched reads — during an in-flight purge and after completion.

Re-arm in `post_upgrade` already covers resuming in-flight purges (ADR 0020).

## Design Documentation Impact

| Document | Update | Status |
|----------|--------|--------|
| [adr/README.md](README.md) | Index ADR 0021 | this patch |
| [adr/0017-graph-vertex-existence-ssot.md](0017-graph-vertex-existence-ssot.md) | Refine incident-edge invariant: tombstoned ⇒ no *visible* incident edges; read-time gate during purge | on acceptance |
| [storage/bulk-ingest-finalize.md](../storage/bulk-ingest-finalize.md) | Delete path: tombstone-first + phased purge work item | on acceptance |
| [architecture/overview.md](../architecture/overview.md) | Note read-time neighbor gate during super-node purge | on acceptance |

## Required Axes Impact (adr-review)

- **Encapsulation:** purge *execution* stays inside the LARA deferred graph
  (owner of tombstones, adjacency, and the queue); the graph facade owns
  *visibility* (pending-purge set + edge-yield gate) and supplies the
  delete-edge/sidecar-clear observer. No internal LARA state is exposed across
  APIs beyond the observer callbacks.
- **Separation of concerns:** execution trigger (timer) and reclamation (queue)
  are reused as-is; the new concern (mid-delete visibility) lives at the graph
  facade read boundary — the same layer that already owns ADR-0017 liveness.
- **Invariants:** the existence invariant is *refined*, not duplicated; it stays
  owned by the graph CSR (tombstone) plus the graph-facade read gate.
- **Consistency:** canonical state = CSR tombstone + LARA queue; the graph-side
  pending-purge bitmap is the membership index, inserted on delete and cleared on
  the observer's purge-completed callback (single logical update path).
- **Fitness for purpose:** solves the concrete super-node trap by extending the
  deferred-maintenance domain; no general-purpose crate gains Gleaph/ICP
  specifics (the gate uses only LARA's own tombstone bit).
