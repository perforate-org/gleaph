# graph-pma Adjacency Next Roadmap

Last updated: 2026-04-03

## Purpose

This document captures the next implementation roadmap for the adjacency
subsystem after the `VertexRef` / `EdgeRef` / `LogicalEdgeLocator` transition.

It is intended as a handoff document for the next agent or engineer who will
continue the work.

Use this document for:

- understanding the current steady-state design direction
- seeing what has already been completed
- identifying what is still transitional
- choosing the next implementation steps in a large chunk instead of as
  isolated micro-edits

Design intent still lives in:

- [`graph-pma-low-level-spec.md`](./graph-pma-low-level-spec.md)
- [`graph-pma-target-design.md`](./graph-pma-target-design.md)

## Target Shape

The adjacency subsystem is moving toward this final shape.

- traversal hot path:
  - `VertexRef -> VertexEntry -> EdgeRef + degree -> EdgeEntry slice -> target: VertexRef`
- overflow hot path:
  - `VertexRef -> VertexEntry.log_offset -> overflow chain`
- mutation path:
  - `EdgeId -> LogicalEdgeLocator`
- allocator/storage path:
  - `segment_id -> SegmentDirectory -> bucket/extent metadata`

The critical design goals are:

- keep traversal DGAP-like and direct
- avoid extra semantic-ID lookups during traversal
- keep each vertex's base neighborhood physically contiguous
- allow segment-local replacement / relocation in stable memory
- keep foreground writes light and push structural cleanup into maintenance

## What Is Already Done

### Low-level identity model

- `VertexRef` is now the low-level vertex identity
- `EdgeRef` is now the low-level packed base-neighborhood reference
- `LogicalEdgeLocator` is the canonical mutation locator
- low-level `NodeId` naming has been pushed to the boundary layers

Primary files:

- [`crates/graph-pma/src/low_level/ids.rs`](../crates/graph-pma/src/low_level/ids.rs)
- [`crates/graph-pma/src/low_level/edge.rs`](../crates/graph-pma/src/low_level/edge.rs)
- [`crates/graph-pma/src/low_level/vertex.rs`](../crates/graph-pma/src/low_level/vertex.rs)

### Mutation locator model

- physical `EdgeLocator` has been removed from production code
- physical `EdgeLocatorSidecar` has been removed
- `EdgeLogicalLocatorSidecar` is the canonical edge-id mapping
- graph replace / tombstone paths are now locator-first

Primary files:

- [`crates/graph-pma/src/low_level/locator.rs`](../crates/graph-pma/src/low_level/locator.rs)
- [`crates/graph-pma/src/low_level/graph.rs`](../crates/graph-pma/src/low_level/graph.rs)

### Foreground/background split

- deferred foreground rebalance policy exists
- timer-driven maintenance queue exists
- queue persistence, checksum, diagnostics, and batch execution exist
- segment lifecycle supports `Active -> Retired -> Free`

Primary files:

- [`crates/graph-pma/src/low_level/graph.rs`](../crates/graph-pma/src/low_level/graph.rs)
- [`crates/graph-pma/src/low_level/manager.rs`](../crates/graph-pma/src/low_level/manager.rs)
- [`crates/graph-pma/src/facade.rs`](../crates/graph-pma/src/facade.rs)

### Runtime storage seam

`SurfaceRuntime` routes base adjacency through `SurfaceBaseStorage`.

Implemented pieces:

- `SurfaceBaseStorage`, `SurfaceBaseSegmentLayout`, `SurfaceBaseSlot`, `SurfaceBaseSpan`
- `SurfaceBaseBacking` (`Contiguous` / `Segmented` + `SegmentedBaseBacking`)
- explicit constructors: `from_contiguous`, `from_segmented`, `from_segmented_with_slot_capacities`
- `EdgeRef -> global slot/span` resolution delegated to the backing
- `window_span_for_vertices` guards against cross-segment window spans
- `SurfaceRuntime::from_decoded_regions`, `set_base_storage`, `replace_base_storage_with_segmented`,
  `migrate_contiguous_base_to_segment_zero`

Primary file:

- [`crates/graph-pma/src/low_level/runtime.rs`](../crates/graph-pma/src/low_level/runtime.rs)

### Segment-aware stable-memory I/O (Phase 4 largely complete)

- `hydrate_surface_runtimes_from_stable_memory` rebuilds `SurfaceBaseStorage` via
  `hydrate_edge_storage_from_stable_memory` (segment 0 payload + explicit edge segments).
- `write_edge_storage_to_stable_memory` flushes each segment to its resolved extent; root
  segment 0 uses the region logical length contract.
- Layout-only `hydrate_surface_runtime` builds **segment 0–only segmented** storage so the
  in-memory shape aligns with the stable-memory path (single flat region encoded as segment 0).

Primary files:

- [`crates/graph-pma/src/low_level/hydration.rs`](../crates/graph-pma/src/low_level/hydration.rs)
- [`crates/graph-pma/src/low_level/manager.rs`](../crates/graph-pma/src/low_level/manager.rs)

### Graph-level migration helpers (Phase 2 graph slice)

- `GraphRuntime::replace_base_storages_with_segmented`
- `GraphRuntime::migrate_contiguous_base_to_segment_zero`

Primary file:

- [`crates/graph-pma/src/low_level/graph.rs`](../crates/graph-pma/src/low_level/graph.rs)

### Segment capacity metadata

- `SurfaceBaseSegmentLayout::sync_slot_capacity_from_manager` no longer depends on a separate
  “total storage length” argument for deriving slot capacity (segment 0 still merges
  with manager-reported capacity via the map).

### Base adjacency indexing and rewrite API surface

- **No subscript access:** the `gleaph-graph-pma` crate has **no** `base_entries[…]` uses; tests and
  production paths use `SurfaceBaseStorage::get`, `replace`, `copied()`, etc.
- **Rewrite entrypoints:** `SurfaceBaseStorage::rewrite_span(start, end, …)` is **`pub(crate)`**;
  other crates and modules should use only `rewrite_vertex_window_span` / `rewrite_span_by_ref`.
- **Segment-zero slot sync call sites:** `sync_segment_zero_slot_capacity_from_storage_len` and its three call
  paths (`push`, crate-private `rewrite_span`, `From<Vec<EdgeEntry>>`) are documented in
  [`runtime.rs`](../crates/graph-pma/src/low_level/runtime.rs), including that **manager/header sync**
  is authoritative when present and that **shrinking** these calls needs an explicit invariant pass
  (no ad hoc removal).
- **Doc + iteration API:** [`SurfaceBaseStorage`](../crates/graph-pma/src/low_level/runtime.rs) has
  struct-level documentation for the rewrite chain (public window/ref entrypoints, crate-private
  `rewrite_span`, link to `GraphRuntime::apply_local_rebalance_delta_with_segment_replacement`) and
  a public [`SurfaceBaseStorage::iter`](../crates/graph-pma/src/low_level/runtime.rs) over flattened
  logical order (prefer over relying on slice `Deref` alone where it clarifies intent).
- **Observability:** maintenance-cycle uses a `window_total_base_slots` label matching the projection
  field; maintenance-batch uses **`maintenance_queue_storage=`** for the queue snapshot tuple (not
  ambiguous `storage=`) ([`observability.rs`](../crates/graph-pma/src/observability.rs)).

## Current Transitional State

### Phase 3: segment-local rewrite as the *canonical* story

**Done mechanically:** segmented backing enforces single-segment local splices for span rewrites;
maintenance can allocate fresh segments and retire old ones (`apply_local_rebalance_delta_with_segment_replacement*`
in `graph.rs`).

**Still transitional:**

- **Known debt (API vs implementation):** `SurfaceBaseStorage::rewrite_span(start, end, …)` is
  **crate-private**; external modules should use `rewrite_vertex_window_span` or
  `rewrite_span_by_ref` only. Internally, splice still runs on **global flattened indices**
  derived from those helpers. A future iteration could make the backing splice ref-native end-to-end.

- **Maintenance narrative:** `GraphRuntime::apply_local_rebalance_delta_with_segment_replacement`
  documents the **canonical** path; [`SurfaceBaseStorage`](../crates/graph-pma/src/low_level/runtime.rs)
  now summarizes the rewrite chain at the type level and links back to that API. **Optional** thin
  helpers that take `EdgeRef + span length` only (no new public global-index APIs) remain future work.
  **Audit (2026-04-03):** still **no** in-crate call sites beyond `rewrite_span_by_ref` itself, so no
  thin wrapper was added.

### Phase 5: remove the flat edge-table worldview

**Still transitional:**

- **Subscript indexing is done** (see “Base adjacency indexing” above). **`Deref` / `DerefMut` to
  `[EdgeEntry]`** still encourages slice habits; **`SurfaceBaseStorage::iter`** is now used explicitly
  in `migrate_contiguous_base_to_segment_zero` and selected tests (graph migrate + runtime
  rebalance snapshot). **Production** paths reviewed: no additional `&[EdgeEntry]` coercion issues
  beyond tests; Facade **`&runtime.base_entries`** for writeback remains typed storage, not slice abuse.
- `sync_segment_zero_slot_capacity_from_storage_len` unchanged in behavior; **doc** now states manager sync
  is authoritative when present and warns against ad hoc removal.
- Diagnostics: **`maintenance_queue_storage=`** on maintenance-batch lines. **2026-04-03:** grep review of
  remaining `insert-edge` / `maintenance-queue*` / `edge` lines — each already carries `path=`, vertex,
  or queue tuple context, so **no further label changes** this iteration (avoid churn to log-string tests).
- **Tests:** `base_entries` assignments in [`facade.rs`](../crates/graph-pma/src/facade.rs),
  [`integration.rs`](../crates/graph-pma/src/integration.rs), and
  [`runtime.rs`](../crates/graph-pma/src/low_level/runtime.rs) tests now use
  **`SurfaceBaseStorage::from_contiguous(vec![…])`** for explicit storage intent; `SurfaceBaseStorage` is
  **re-exported** from [`low_level/mod.rs`](../crates/graph-pma/src/low_level/mod.rs) for same-crate use.

### Facade in-memory shape (policy)

The low-level graph can still start life with **contiguous** backing in some construction paths.
**Policy:** `GraphPma` does **not** auto-call `migrate_contiguous_base_to_segment_zero`
today; callers that need a uniform segmented in-memory representation should invoke it explicitly
after construction or in a dedicated normalization step. Revisit when all bootstrap paths emit
segmented storage by construction.

## Immediate Roadmap (status)

| Phase | Goal | Status |
|-------|------|--------|
| 1 | `SurfaceBaseStorage` as full storage authority | Mostly done; **subscript `base_entries[i]` cleanup is complete**; remaining: reduce **slice/`Deref`** habits in non-test code where practical |
| 2 | Segmented backing in real paths | Done for stable-memory + layout hydrate + graph helpers |
| 3 | Segment-local replacement as canonical rewrite | Partially done; **`rewrite_span` is `pub(crate)`**; **`SurfaceBaseStorage` doc** links maintenance to segment replacement; **optional `EdgeRef`-only thin wrappers** still open |
| 4 | Segment-aware writeback/hydration | Done for stable-memory multi-segment flush/load |
| 5 | Remove contiguous fallbacks | In progress (**explicit `iter` in migrate + tests**; **`maintenance_queue_storage=`**; test **`from_contiguous`** for `base_entries`; observability strings reviewed, **no change**; **segment-zero slot sync doc**; optional further **`Deref` cleanup**) |

### Phase 3 (next implementation focus)

Goal:

- stop thinking of rebalance as splicing one flat edge array at the API/documentation level

Tasks:

- **Done on `graph.rs`:** `apply_local_rebalance_delta_with_segment_replacement` documents the
  **canonical** maintenance path (fresh segments + segment-local windows); keep that narrative
  when touching related APIs.
- prefer `rewrite_vertex_window_span` / `rewrite_span_by_ref` in new code (global-index
  `rewrite_span` is crate-private)
- **Done:** struct-level documentation on `SurfaceBaseStorage` in
  [`runtime.rs`](../crates/graph-pma/src/low_level/runtime.rs) linking to
  `GraphRuntime::apply_local_rebalance_delta_with_segment_replacement`
- optionally add thin wrappers that take `EdgeRef + span length` only (hide global indices)

Success condition:

- maintenance-driven structural change is explainable as **segment-local window + segment replacement**
  without referring to a single global flat array

Primary files:

- [`crates/graph-pma/src/low_level/runtime.rs`](../crates/graph-pma/src/low_level/runtime.rs)
- [`crates/graph-pma/src/low_level/graph.rs`](../crates/graph-pma/src/low_level/graph.rs)

### Phase 5 (parallel track)

Goal:

- segmented backing is the mental model everywhere, not only inside `SurfaceBaseStorage`

Tasks:

- **Progress:** [`SurfaceBaseStorage::iter`](../crates/graph-pma/src/low_level/runtime.rs) used in
  `migrate_contiguous_base_to_segment_zero`, graph migrate test, and one runtime rebalance
  test (replacing `&*base_entries` / implicit `Deref` iteration there)
- **Inventory (production):** no further mandatory changes found; continue preferring `iter` / `get`
  when touching code; Facade stable-memory paths use `SurfaceBaseStorage` by reference as intended
- **`sync_segment_zero_slot_capacity_from_storage_len`:** behavior unchanged; **expanded rustdoc** on when
  manager sync wins and why not to delete call sites casually
- **Observability:** maintenance-batch **`maintenance_queue_storage=`** label; **2026-04-03 audit:** other
  maintenance/edge strings left unchanged (already disambiguated by path/vertex/queue fields).
- **Tests (2026-04-03):** `base_entries` test fixtures use **`SurfaceBaseStorage::from_contiguous`**
  instead of `vec![…].into()`; `SurfaceBaseStorage` added to **`low_level` re-exports**.

Primary files:

- [`crates/graph-pma/src/facade.rs`](../crates/graph-pma/src/facade.rs)
- [`crates/graph-pma/src/low_level/runtime.rs`](../crates/graph-pma/src/low_level/runtime.rs)
- [`crates/graph-pma/src/observability.rs`](../crates/graph-pma/src/observability.rs)

## Invariants To Preserve

The next agent should preserve these invariants while implementing the roadmap.

- traversal must not require `EdgeId` lookups
- `EdgeEntry.target` remains `VertexRef`
- `VertexEntry.edge_ref` remains the base-neighborhood anchor
- each vertex's base neighborhood remains physically contiguous within its
  segment
- `LogicalEdgeLocator` remains the mutation-side canonical locator
- foreground writes stay light; hard structural cleanup stays maintenance-first
- `segment 0` remains the root flat segment until segmented persistence
  fully replaces the old model

## Recommended Next Concrete Step

1. **Phase 3 (optional):** Add **`EdgeRef + span`-only** thin wrappers only when **multiple** new
   call sites repeat the same pattern. **2026-04-03:** still **only** the `rewrite_span_by_ref` definition
   (no extra in-crate call sites) — **skip** until a repeated pattern appears. Keep **`rewrite_span`
   `pub(crate)`** and no new public global-index rewrite APIs.
2. **Phase 5:** Optional remaining **`Deref` / slice** habits in tests; deliberate
   **`sync_segment_zero_slot_capacity_from_storage_len`** vs **manager-backed** change only after an invariant
   write-up. **Observability:** further **edge-segment** vocabulary only if new ambiguity shows up in
   production logs (current strings reviewed 2026-04-03).
3. **Optional:** If a single in-memory shape is required at bootstrap, call
   `GraphRuntime::migrate_contiguous_base_to_segment_zero` from one facade hook **after**
   policy is agreed (currently intentionally not automatic).
