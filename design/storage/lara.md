# LARA: Localized Adjacency Relocation Array

**Status:** accepted  
**Crate:** `ic-stable-lara`  
**Reference:** [DGAP](https://github.com/DIR-LAB/DGAP) (`reference/DGAP/`)  
**Detail:** [lara-dgap-contract.md](./lara-dgap-contract.md) · [lara-and-facade.md](./lara-and-facade.md)

## Purpose

Define the **agreed LARA storage model** for Gleaph: what LARA is, how it relates to DGAP, and which parts are LARA design vs implementation substrate.

## Non-goals

- Byte-level stable memory layouts (see crate module docs).
- Gleaph facade / federation (see [lara-and-facade.md](./lara-and-facade.md)).
- Labeled migration plan (see [ADR 0001](../adr/0001-labeled-segment-slide.md)).

---

## What LARA is

**LARA** is a mutable CSR adjacency store that keeps **scan paths direct** while allowing **local physical relocation** of dense adjacency regions.

Name breakdown:

| Term | Meaning |
|------|---------|
| **Localized** | Rebalance and relocate work on a PMA window (leaf or ancestor window), not the whole graph on every insert |
| **Adjacency** | CSR-style out-edge lists on a shared edge slab |
| **Relocation** | Physical edge bytes move; vertex rows are rewritten |
| **Array** | Contiguous edge slab + explicit metadata stores |

LARA is a **storage algorithm and contract**, not an Internet Computer feature. The `ic-stable-lara` crate implements LARA on IC stable memory because Gleaph runs on canisters — the same contracts could be implemented on other persistent backends.

---

## Relationship to DGAP

[DGAP](https://github.com/DIR-LAB/DGAP) (Dynamic Graph Adjacency structure on PM) is the primary external reference for:

- Per-vertex scan row (`index`, `degree`, `offset` / log head)
- PMA segment tree and leaf density (`actual` / `total`)
- Weighted rebalance inside a fixed physical window (`rebalance_weighted`)
- Per-segment overflow logs when the slab window is full

**LARA adopts DGAP semantics for scan and in-window slide.** It **extends** DGAP with explicit structures for incremental physical relocation and reuse. Those extensions are **part of LARA**, not IC quirks.

```text
DGAP                          LARA (adds)
────────────────────────────────────────────────────────────
vertex_element                Vertex / CsrVertex row (same role)
edges_[] slab                 EdgeStore slab
PMA leaf [from, to)             segment_edges_actual / total + span_meta
rebalance_weighted (in-window)  rebalance_weighted_with_layout
resize_V1 (global grow)         elem_capacity growth + segment relocate
(implicit slack in segment)     FreeSpanStore (retired physical reuse)
                                segment_slide + adjacent-buffer coalesce
```

---

## Four contracts (consensus)

All LARA code — core and labeled — must respect these boundaries.

### 1. Scan contract

**Who:** iterators, planners, read-only graph APIs.

**May read:** vertex row fields needed for visibility (`base_slot_start`, live `degree`, `log_head` / bucket descriptors).

**Must not read:** PMA counts, `SegmentSpanMetaStore`, `FreeSpanStore`, maintenance flags.

**Path:** `vertex_id → row → live slab prefix (+ overflow log chain)`.

### 2. Vertex-local update contract

**Who:** insert, tombstone, per-vertex packed moves.

**Geometry:** CSR successor boundary inside a PMA leaf (next vertex `base_slot_start`, leaf `total`, slab `elem_capacity`).

**Overflow:** per-leaf segment log when the in-window slab is full.

**Slack:** tombstones and `stored_degree` vs live `degree` until rebalance packs the row.

Same role as DGAP `do_insertion` + `have_space_onseg`.

### 3. Segment physical contract (rope)

**Who:** density cascade, weighted rebalance, segment relocate, segment slide.

**Unit:** PMA leaf physical block — up to `segment_size` vertices (default 32) sharing one assigned width on the edge slab.

**In-window:** `rebalance_weighted` redistributes live edges and slack **inside** the leaf's current `[physical_start, physical_start + total)` without retiring physical ranges.

**Out-of-window:** when the leaf must move or grow beyond its assignment, physical relocation runs as a committed multi-step update (rewrite vertex bases → update span meta → fold logs → **then** retire old physical range).

This is the **rope**: the leaf physical interval, not individual vertex rows.

### 4. Free-span contract (core LARA)

**`FreeSpanStore` is a first-class LARA component**, not an IC-specific extension.

**Role:** index of **retired physical edge-slab ranges** that update code may allocate from (best-fit, coalescing with neighbors).

**When spans enter the store:**

| Event | Retire to free span? |
|-------|----------------------|
| Segment relocate / slide completes | **Yes** — old `[physical_start, physical_start + total)` |
| Weighted rebalance inside fixed leaf capacity | **No** — slack stays inside the leaf assignment |
| Per-vertex degree growth within CSR window | **No** — append, tombstone reuse, or in-place pack |
| Global `elem_capacity` growth | Optional tail; may also coalesce from free list |

**Why LARA has this and DGAP does not (as explicitly):** DGAP often recovers space through `resize_V1` and implicit segment totals on a PMEM heap. LARA targets **incremental** relocation — `segment_slide`, in-place expansion into adjacent free gaps (`try_expand_segment_in_place`), and reuse without rewriting the entire slab on every cascade. The free-span store is the retirement pool that makes localized relocation safe and reusable.

**Commit order invariant** (from `lara.rs`): relocate and rewrite all live pointers first; **only then** `release_span` old physical ranges. Queries never observe free-span slots as live adjacency.

**Labeled note:** per-vertex `release_vertex_edge_span_footprint` on routine growth is **not** this contract; see [ADR 0001](../adr/0001-labeled-segment-slide.md).

---

## LARA stores (edge slab side)

| Store | Contract | Scan? |
|-------|----------|-------|
| `EdgeStore` | Live edge bytes | Yes (via vertex row) |
| `counts_store` | PMA `actual` / `total` per tree node | No |
| `log` | Per-leaf overflow entries | Yes (via `log_head` only) |
| `span_meta` | Leaf `physical_start` when order breaks | No |
| `free_spans` / `free_span_by_start` | Retired physical ranges | No |

---

## What is IC-specific (substrate only)

These are **implementation choices for Gleaph on canisters**, not part of the LARA algorithm definition:

- `ic-stable-structures::Memory` and stable memory region wiring
- Canister upgrade / persistence lifecycle
- `canbench` / Wasm benchmark harness

Changing substrate (e.g. host-side persistent mmap) should preserve the four contracts above.

---

## Labeled LARA (current alignment)

| Layer | Status |
|-------|--------|
| Scan (`LabelBucket`, `LabelEdgeSpan`) | Aligned with DGAP vertex + per-label windows |
| Overflow logs | Aligned (shared per-leaf log) |
| Segment physical (rope) for **edge bytes** | **Implemented** — PMA leaf block per [ADR 0001](../adr/0001-labeled-segment-slide.md); per-vertex sub-ranges inside pinned leaf |
| Free-span usage for labeled edge bytes | **Implemented** — segment footprint on leaf relocate; per-vertex peel only for unpinned legacy spans |

Payload slab ([labeled-edge-payloads.md](./labeled-edge-payloads.md)) follows the same logical compaction order as edge bytes; physical alignment with leaf rope is part of the labeled migration.

---

## Consensus checklist

Use this when reviewing LARA PRs:

- [ ] Scan paths do not touch `span_meta` or `FreeSpanStore`
- [ ] In-window rebalance does not `release_span`
- [ ] Segment relocate releases **one** retired leaf footprint after commit
- [ ] `FreeSpanStore` allocation is best-fit / coalesce, not scan-visible
- [ ] Labeled changes do not deepen per-vertex tail-append + peel without ADR exception

---

## Related documents

- [lara-dgap-contract.md](./lara-dgap-contract.md) — DGAP mapping and labeled gap detail
- [adr/0001-labeled-segment-slide.md](../adr/0001-labeled-segment-slide.md) — labeled physical migration
- [lara-labeled-migration-tests.md](./lara-labeled-migration-tests.md) — phase test gates (A–E)
- `crates/ic-stable-lara/README.md` — crate entry point
- `reference/DGAP/dgap/src/graph.h` — reference implementation
