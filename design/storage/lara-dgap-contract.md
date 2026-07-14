# LARA storage contract (DGAP alignment)

**Status:** Partially Implemented (core contract: [lara.md](./lara.md) accepted)  
**Last updated:** 2026-07-13
**Anchor timestamp:** 2026-07-13 22:46:07 UTC +0000
**Reference:** [DGAP](https://github.com/DIR-LAB/DGAP) (`reference/DGAP/dgap/src/graph.h`), [arXiv:2403.02665](https://arxiv.org/abs/2403.02665)  
**Source of truth:** `crates/ic-stable-lara/` (`lara.rs`, `lara/edge.rs`, `labeled.rs`)

## Purpose

State how **LARA** maps the DGAP dynamic CSR model to Internet Computer stable memory, and where **labeled** extensions diverge today.

## Non-goals

- PMA density proofs or full byte layouts (see `crates/ic-stable-lara/README.md`).
- DGAP crash-consistency / PMDK details (Gleaph uses `ic-stable-structures`, not PMEMobj).

---

## DGAP in one page

DGAP keeps a **mutable CSR** on persistent memory:

| Object | Role |
|--------|------|
| `vertex_element` | Per-vertex **scan metadata**: `index` (slab start), `degree`, `offset` (overflow-log head, `-1` = slab-only) |
| `edges_[]` | Global edge slab |
| PMA leaf segment | `segment_size` vertices; `segment_edges_actual` (live edges) vs `segment_edges_total` (assigned physical width) |
| Per-segment log | Overflow when `index + degree` hits the next vertex boundary |
| `rebalance_weighted` | **Rope-style slide** inside a vertex window `[start_vertex, end_vertex)` — proportional gaps by `degree + 1`, rewrite `index`, fold logs |
| `resize_V1` | Grow `elem_capacity` when no PMA window can absorb density; re-place all vertices |

**Important:** DGAP does **not** maintain a separate free-span index. Slack lives **inside** each segment's assigned physical interval `[vertices[s·S].index, vertices[(s+1)·S].index)`. Rebalance **slides** data within that interval; growth extends `elem_capacity`.

---

## LARA layering (target contract)

LARA ports DGAP's split and adds **LARA structures** (not IC-specific; see [lara.md](./lara.md)):

```text
┌─────────────────────────────────────────────────────────────────┐
│ Scan contract (DGAP vertex row)                                   │
│   Vertex / LabeledVertex: base_slot_start, degree, stored_*     │
│   Walk live slab prefix + optional overflow log                   │
│   MUST NOT read: PMA counts, span_meta, free_span               │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Update contract — vertex-local (DGAP)                             │
│   Insert in CSR window (successor base_slot_start / leaf total)   │
│   Overflow → per-leaf segment log                                 │
│   Tombstones, degree/stored_degree, in-place packed moves       │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Update contract — segment physical (Rope / PMA)                   │
│   PMA leaf: segment_size vertices (default 32)                    │
│   segment_edges_actual / segment_edges_total → density            │
│   rebalance_weighted: slide + redistribute slack inside window    │
│   segment_slide / relocate: move leaf physical block, rewrite     │
│     all affected vertex base_slot_start values                    │
│   span_meta.physical_start: leaf slab pin when CSR order breaks   │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Free-span store (core LARA — see lara.md § Free-span contract)    │
│   Retired **physical** slot ranges after segment relocate/slide     │
│   NOT for in-window rebalance or per-vertex routine growth        │
│   Best-fit reuse; adjacent coalesce for segment_slide             │
└─────────────────────────────────────────────────────────────────┘
```

### Boundary matrix

| Storage behavior | DGAP | Core LARA | Labeled (target) | Labeled (today) |
|------------------|------|-----------|------------------|-----------------|
| Scan adjacency | `index`, `degree`, `offset` | `base_slot_start`, `degree`, `log_head` | + `LabelBucket` rows | Implemented |
| Insert slack in window | CSR successor boundary | Same (`row_layout`) | Per-label slack inside vertex span | Implemented |
| Overflow | Per-leaf log | Per-leaf log | Per-leaf log (shared) | Implemented |
| Density trigger | PMA `actual/total` | Same (`counts_store`) | Leaf cascade (partial) | Partial |
| Physical slide | `rebalance_weighted` in `[from,to)` | `rebalance_weighted_with_layout` | Leaf weighted slide in pinned block | Implemented |
| Physical growth | `resize_V1` | `elem_capacity` + segment relocate | Leaf resize / relocate | Leaf slide / relocate (no steady-state tail append) |
| Retire old physical | (implicit in resize; no free list) | `FreeSpanStore` after relocate | Segment footprint only | One `release_span` per leaf relocate |
| Multi-label bytes | N/A | N/A | DGAP vertex rows + buckets; **one leaf physical block** | Sub-ranges inside pinned leaf block |

Inline values are a separate physical domain: bucket-local live order associates values with edges, while payload slab slots, payload log entries, capacity, and maintenance timing remain independent from the edge leaf block.

---

## Core LARA (implemented, DGAP-aligned)

**Crate modules:** `lara.rs`, `lara/edge/*`, `lara/vertex/*`

- **Vertex row** matches DGAP `vertex_element`: locator packs `base_slot_start`; `degree` / `stored_degree` play the live vs reserved roles; `log_head` ≡ `offset`.
- **PMA segment tree:** `segment_count`, `segment_size`, `segment_edges_actual`, `segment_edges_total` in `counts_store` — same density semantics as DGAP (`reference/DGAP/dgap/src/graph.h`, `recount_segment_*`).
- **Weighted rebalance:** `rebalance_weighted_with_layout` redistributes edges and slack across a vertex index range inside the segment's physical capacity — DGAP `spread_weighted` / `rebalance_weighted`.
- **Segment relocation:** `segment_slide`, hot-segment relocate tests — physical block moves; vertex bases rewritten; then old physical range → **`FreeSpanStore`** (core LARA; DGAP often folds equivalent space recovery into `resize_V1` instead of an explicit retirement pool).
- **Scan isolation:** Documented in `lib.rs` and `lara/edge.rs`; iterators must not touch `span_meta` or `free_spans`.

---

## Labeled extension

### Scan / DGAP vertex semantics (implemented)

Normal labeled vertices:

- `LabeledVertex.degree` — live `LabelBucket` count (not edge count).
- `LabelBucket` — per-label `edge_start`, `degree`, `stored_slots`, `overflow_log_head`.
- `LabelEdgeSpan` scan uses bucket + successor boundary (same CSR-window idea as DGAP, scoped to one label).

Default-label bypass uses core `Vertex` row semantics directly.

### Physical edge bytes (partial — known drift)

**Target (DGAP + LARA):**

- One **PMA leaf physical reservation** holds edge bytes for up to `segment_size` vertices.
- Each vertex's labels occupy **sub-ranges** inside that leaf block (proportional slack by label degree), analogous to DGAP vertices sharing one segment `[from, to)`.
- When the leaf is dense, **`rebalance_weighted` / segment_slide** on the leaf window moves the whole block; overflow logs fold; old physical span → free list.
- **No per-vertex `release_span` on routine growth.**

**Today (implemented — `labeled/graph/compact.rs`):**

- Labeled edge bytes are pinned to PMA leaf blocks (`span_meta.physical_start`).
- Growth uses in-leaf weighted slide and leaf-block relocate; one `release_span` per relocate.
- `LabeledVertex.stored_slots` tracks the per-vertex sub-range width inside the leaf block; routine insert does not tail-append at `elem_capacity`.
- `rebalance_vertex_edge_span` and `rewrite_vertex_edge_span` resolve bases via leaf relocate when pinned; tail append is limited to unpinned rows and relocate-internal escape hatches.

---

## Free span: when to use

| Operation | Use free span? |
|-----------|----------------|
| Segment relocate / slide completes | **Yes** — release old `[physical_start, physical_start + total)` |
| DGAP-style rebalance inside fixed segment capacity | **No** — slack stays inside segment assignment |
| Per-vertex degree growth within window | **No** — append or tombstone reuse |
| Labeled leaf-block relocate (pinned) | **Yes** — one `release_span` per leaf relocate |

---

## Rope analogy

| Rope | LARA / DGAP |
|------|-------------|
| Chunk of bytes | PMA leaf physical slab block (`segment_edges_total`) |
| Rebalance / split | `rebalance_weighted` sliding edges within `[from, to)` |
| Grow document | `resize_V1` / `elem_capacity` growth + full re-place |
| Free chunk list | LARA `FreeSpanStore` (core LARA; after segment relocate, not per in-window rebalance) |
| Per-character metadata | Per-vertex `base_slot_start` / `index` (DGAP vertex row) |

**Vertex-level metadata is not the rope.** The rope is the **segment physical interval** on the edge slab.

---

## Migration direction (labeled → contract)

Ordered steps; each should keep `mixed_label_hub_*` regressions green:

1. **Pin labeled edge bytes to PMA leaf `span_meta.physical_start`** — vertices in a leaf share one contiguous physical block (like DGAP `recount_segment_total`).
2. **Drive density / cascade from leaf `segment_edges_actual/total`**, not isolated `VertexEdgeSpan` rewrite.
3. **On leaf rebalance:** fold label overflow logs, run weighted slide on the leaf vertex range, update bucket `edge_start` values — mirror `rebalance_data_V1`.
4. **Retire old leaf physical block via single `release_span`** (segment footprint), not per-vertex peel.
5. **Remove per-vertex `stored_slots` append-at-tail** for normal labeled rows once leaf slide covers growth (bypass mode may keep core vertex path).

**Failure-atomic growth and promotion.** Core LARA `grow_segment_tree_to` and labeled `promote_bypass_to_bucket_mode` use a preflight/commit split: all fallible backing-memory growth (`counts_store`, `span_meta`, `log`, bucket slab, free-span records, free-span by-start index) completes before the first canonical metadata write. After that point no recoverable `Memory::grow` error remains. Physical preallocation is non-canonical and safe to retain after a rejected mutation.

**Status:** Phase A–E implemented (pinned leaf, PMA density, in-window slide, single leaf `release_span` on relocate, rewrite-path growth via leaf relocate). Failure-atomic preflight is implemented for `grow_segment_tree_to` and `promote_bypass_to_bucket_mode`. New buckets use zero-length edge placement and the edge log. Inline values persist their own slab-slot count and maintain an independent slab/log lifecycle.

---

## Related documents

- [lara.md](./lara.md) — **agreed LARA model** (four contracts, DGAP vs LARA)
- [0001-labeled-segment-slide.md](../adr/0001-labeled-segment-slide.md) — decision record
- [0016-overflow-log-tombstones-and-src-fields.md](../adr/0016-overflow-log-tombstones-and-src-fields.md) — historical edge-log tombstone and payload cell/blob layout decisions; ADR 0001 now owns independent edge/payload maintenance
- [lara-and-facade.md](./lara-and-facade.md) — Gleaph layering
- [labeled-edge-inline-values.md](./labeled-edge-inline-values.md) — payload slab (parallel contract)
- `crates/ic-stable-lara/README.md` — crate-level overview
- `reference/DGAP/dgap/src/graph.h` — reference implementation (~1500 LOC)
