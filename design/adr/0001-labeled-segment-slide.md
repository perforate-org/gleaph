# 0001. Labeled edge physical layer uses PMA leaf segment slide

Date: 2026-06-07  
Status: accepted

## Context

**Core LARA** (`crates/ic-stable-lara/src/lara.rs`) already follows the DGAP dynamic CSR model on IC stable memory:

- Per-vertex scan metadata (`base_slot_start`, `degree`, `log_head`) — DGAP `vertex_element`.
- PMA leaf density (`segment_edges_actual` / `segment_edges_total`) and weighted rebalance inside a fixed physical window.
- Segment relocation / slide when density or capacity requires moving a leaf block; retired physical ranges go to `FreeSpanStore`.

**Labeled LARA** adds multi-label adjacency (`LabeledVertex`, `LabelBucket`, per-label `LabelEdgeSpan`). Scan semantics and overflow logs align with DGAP. **Physical edge bytes do not.**

Today, labeled rows reserve a per-vertex `VertexEdgeSpan` (`LabeledVertex.stored_slots`) on the shared edge slab. Growth and rebalance in `labeled/graph/compact.rs` often:

1. Append a new span at `elem_capacity` (tail).
2. Retire the old footprint via `release_vertex_edge_span_footprint` (monolithic or bucket+gap intervals).

This is an **interim** layout documented in `labeled.rs` and [lara-dgap-contract.md](../storage/lara-dgap-contract.md). It treats each vertex as its own physical rope chunk instead of sharing one PMA leaf block.

**Observed failure mode:** On a high-degree, many-label hub (e.g. 50k edges across ~33 labels), a single label-bucket rebalance triggered per-vertex span release with O(n) slab peeling — ~23 minutes wall time. Root cause was not PMA log folding but **per-vertex free-span retirement** on a path that DGAP never uses for routine insert.

A reference implementation lives at `reference/DGAP/dgap/src/graph.h`. DGAP keeps slack **inside** each leaf segment's assigned `[from, to)` interval and slides data with `rebalance_weighted`; it does not peel per-vertex footprints on every growth event.

Payload storage ([labeled-edge-inline-values.md](../storage/labeled-edge-inline-values.md)) already assumes edge compaction follows a **single logical order** across the edge slab. Divergent physical units (per-vertex span vs leaf segment) increase compaction risk and maintenance surface.

## Decision

**Adopt the core LARA / DGAP physical contract for labeled edge bytes.**

1. **Physical unit = PMA leaf block.** Up to `segment_size` vertices (default 32) in one leaf share one contiguous edge-slab reservation pinned by `span_meta.physical_start` and `segment_edges_total`.
2. **Density and rebalance** for labeled edge bytes are driven by leaf PMA counts, not isolated `VertexEdgeSpan` rewrite as the primary maintenance path.
3. **Weighted slide** (`rebalance_weighted_with_layout` / segment slide) redistributes labeled edge bytes and proportional label slack **inside the leaf window**, folding per-leaf overflow logs — mirroring DGAP `rebalance_data_V1`.
4. **Free-span release** applies to **retired leaf physical footprints** after segment relocate or slide completes — **not** per-vertex peel on routine labeled insert or bucket growth.
5. **Scan contract unchanged.** Queries continue to use `LabelBucket` + `LabelEdgeSpan` (and bypass rows for default label); they must not read `span_meta` or free spans.
6. **Interim per-vertex span path** remains only until migration steps land; new features must not deepen dependence on tail-append + per-vertex `release_span` as the steady-state growth model.

Phased delivery (see [lara-dgap-contract.md § Migration](../storage/lara-dgap-contract.md#migration-direction-labeled--contract)):

| Phase | Outcome                                                        |
| ----- | -------------------------------------------------------------- |
| A     | Pin labeled edge bytes to leaf `span_meta.physical_start`      |
| B     | Leaf PMA density triggers labeled maintenance                  |
| C     | Leaf rebalance: slide + log fold + bucket `edge_start` rewrite |
| D     | Single segment `release_span` on relocate                      |
| E     | Remove per-vertex tail append for normal labeled rows          |

Each phase keeps existing `mixed_label_hub_*` regressions green.

## Consequences

### Positive

- Aligns labeled maintenance with proven DGAP/LARA complexity: slide inside fixed capacity vs repeated global tail append.
- Removes cliff class where multi-label hubs pay O(edges) free-span work per vertex relocate.
- One physical story for edge slab and payload compaction order.
- Reviewers can judge labeled PRs against a single contract ([lara-dgap-contract.md](../storage/lara-dgap-contract.md)).

### Negative / cost

- **Large refactor** across `labeled/graph/{compact,insert,bucket}.rs` and rebalance call graph.
- Multi-label **proportional slack** inside a leaf is harder than single-label DGAP; must preserve per-label CSR windows within the shared leaf block.
- Migration is **multi-PR**; interim and target paths may coexist briefly — requires clear feature flags or phase gates in tests.
- Stable-memory **on-disk layout** may need a version bump or one-time rewrite if existing graphs cannot be reinterpreted as leaf-pinned blocks (to be confirmed in Phase A spike).

### Neutral

- Default-label **bypass** rows may continue using core `Vertex` semantics where they already share the unlabeled path.
- `FreeSpanStore` remains core LARA (see [lara.md](../storage/lara.md)); in-window rebalance still does not retire spans — only segment relocate / slide does.

## Alternatives considered

### A. Keep per-vertex `VertexEdgeSpan` (status quo + optimize peel)

- **Pros:** Smaller immediate change; recent footprint fix unblocks correctness.
- **Cons:** Does not match DGAP/LARA rope model; hub-scale cost remains structurally likely; duplicates physical logic core LARA already implements.
- **Rejected** as steady-state architecture. Acceptable only as interim with explicit sunset (this ADR).

### B. Per-label physical spans (one slab reservation per label per vertex)

- **Pros:** Simpler mental model per label.
- **Cons:** Multiplies PMA units and free-span events by label count; worse on many-label hubs; diverges further from DGAP segment model.
- **Rejected.**

### C. Global `resize_V1`-only growth (no segment slide, full re-place on pressure)

- **Pros:** Matches DGAP when local windows cannot absorb density.
- **Cons:** Full-graph re-place is expensive on IC; LARA already invests in segment slide and free-span reuse to avoid this on every cascade.
- **Rejected** as the **only** strategy. Keep as escalation when leaf + window rebalance exhaust capacity (same as core LARA).

### D. Separate edge slab per label

- **Pros:** No cross-label physical coupling.
- **Cons:** Memory overhead, no shared PMA amortization, complicates federation and iterators.
- **Rejected.**

## References

- [lara-labeled-migration-tests.md](../storage/lara-labeled-migration-tests.md) — phase test gates
- [lara-dgap-contract.md](../storage/lara-dgap-contract.md)
- [lara-and-facade.md](../storage/lara-and-facade.md)
- `reference/DGAP/dgap/src/graph.h` — `rebalance_weighted`, `rebalance_data_V1`, `vertex_element`
- `crates/ic-stable-lara/src/labeled/graph/compact.rs` — interim per-vertex span release
