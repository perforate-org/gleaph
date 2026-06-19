# 0021. Resumable super-node DETACH DELETE (deferred incident-edge purge)

Date: 2026-06-19
Status: proposed
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

### 2. Source-tombstone read gate (strengthen)

A tombstoned vertex must yield **no** out-edges regardless of `stored_degree`
(today the early-return in `lara/edge/scan.rs` also requires `stored_degree == 0
&& log_head < 0`). This makes `v`'s own outgoing/reverse edges invisible in O(1)
the instant `v` is tombstoned, without touching them.

### 3. Neighbor read gate (new, gated)

When iterating edges, skip any edge whose **neighbor** vertex row is a tombstone,
**but only while the pending-purge set is non-empty**. The filter lives at the
single LARA traverse chokepoint where edges are yielded, so every consumer
(expand, WCOJ, edge-property reads, federation expand) is covered with one
change. In steady state (no in-flight super-node delete) the gate short-circuits
on `pending_purge.is_empty()` and the per-edge check is never paid.

### 4. Pending-purge set (new stable state)

A stable `StableRoaringBitmap` of local `VertexId`s that are tombstoned but not
yet fully purged. It is the **canonical** "mid-delete" set: insert on delete
commit, remove when the `DeleteVertex` job finishes its last phase. It both
drives the read gate and survives upgrades (a super-node purge in progress must
resume after upgrade — the ADR-0020 timer is re-armed in `post_upgrade`). The
maintenance queue holds the *work*; the bitmap is the *fast membership index*
for reads, maintained on the same single update path as the work item to avoid a
second source of truth.

### 5. Labeled purge work item

Add `MaintenanceWorkItem::DeleteVertex` to the labeled enum and a phased
`process_delete_vertex` mirroring the non-labeled implementation
(`RemoveOutgoing → ClearForwardRow → RemoveIncoming → ClearReverseRow`), bounded
by `max_delete_edge_steps`, invoking the delete-edge observer per removed edge,
and removing `vid` from the pending-purge set on completion.

## Consequences

### Positive
- Super-node `DETACH DELETE` no longer traps; it always makes bounded progress
  and completes across timer ticks.
- Reuses ADR-0020 queue/timer and the proven non-labeled phased-delete algorithm;
  no new execution trigger or subsystem.
- Small deletes are unchanged (finish in the first bounded pass; no pending entry,
  no read-filter cost, immediate consistency).
- One read-filter chokepoint (LARA traverse) keeps visibility correct for all
  query operators via a single, testable gate.

### Trade-offs
- **New stable state** (pending-purge bitmap) and a **new stable work-item
  variant** — additive, no repack, but a stable-format addition that must be
  initialized on existing canisters (empty set / no in-flight jobs).
- **Read-path gate**: a branch per expand call always; a per-edge neighbor
  tombstone check only while a super-node purge is in flight. Net steady-state
  cost ≈ one bool check; bounded worst case during purge windows.
- **Invariant refinement**: ADR 0017's "tombstoned ⇒ no incident edges" becomes
  "tombstoned ⇒ no *visible* incident edges"; the read-time gate is now part of
  the existence SSOT. ADR 0017 must be updated.
- Reclamation is eventually-consistent for super-nodes: between commit and the
  final tick, dangling back-edges physically exist (but are filtered).

## Migration

1. Add the labeled `MaintenanceWorkItem::DeleteVertex` variant + phased
   `process_delete_vertex` + delete-edge observer (mirror non-labeled).
2. Add the stable pending-purge `StableRoaringBitmap` (new MemoryId; empty on
   existing canisters) and `has_pending_vertex_purges()` / membership accessors.
3. Strengthen the source-tombstone early-return in `lara/edge/scan.rs`.
4. Add the gated neighbor-tombstone filter at the LARA traverse yield points.
5. Rewrite labeled `delete_vertex_deferred` to tombstone-first + enqueue +
   bounded inline pass; move per-edge sidecar clearing into the observer in the
   graph facade.
6. Keep a bounded **safety-floor** error if a single purge step cannot advance.
7. Re-arm in `post_upgrade` already covers resuming in-flight purges (ADR 0020).

## Design Documentation Impact

| Document | Update | Status |
|----------|--------|--------|
| [adr/README.md](README.md) | Index ADR 0021 | this patch |
| [adr/0017-graph-vertex-existence-ssot.md](0017-graph-vertex-existence-ssot.md) | Refine incident-edge invariant: tombstoned ⇒ no *visible* incident edges; read-time gate during purge | on acceptance |
| [storage/bulk-ingest-finalize.md](../storage/bulk-ingest-finalize.md) | Delete path: tombstone-first + phased purge work item | on acceptance |
| [architecture/overview.md](../architecture/overview.md) | Note read-time neighbor gate during super-node purge | on acceptance |

## Required Axes Impact (adr-review)

- **Encapsulation:** purge logic stays inside the LARA deferred graph (owner of
  tombstones and the queue); the graph facade only supplies the sidecar-clear
  observer. No internal state is exposed across APIs.
- **Separation of concerns:** execution trigger (timer) and reclamation (queue)
  are reused as-is; the new concern (mid-delete visibility) is localized to one
  read gate + one membership set.
- **Invariants:** the existence invariant is *refined*, not duplicated; it stays
  owned by the graph CSR (tombstone) plus the LARA read gate. The pending-purge
  set has a single update path co-located with the work item.
- **Consistency:** canonical state = CSR tombstone + queue; the bitmap is the
  membership index updated on the same path (no drift surface).
- **Fitness for purpose:** solves the concrete super-node trap by extending the
  deferred-maintenance domain; no general-purpose crate gains Gleaph/ICP
  specifics (the gate uses only LARA's own tombstone bit).
