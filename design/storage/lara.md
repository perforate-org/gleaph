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

**Failure-atomic stable mutations.** Two owner-level mutations are split into an infallible validation phase, a preflight phase that only grows backing memory, and a commit phase that publishes logical metadata:

1. `EdgeStore::grow_segment_tree_to` reserves `counts_store`, `span_meta`, and overflow-log capacity before it migrates counts, appends span-meta rows, resets new log indexes, and writes the edge header.
2. `LabeledLaraGraph::promote_bypass_to_bucket_mode` reserves bucket-slab and free-span capacity (via `LabelBucketStore::plan_promote_bypass_to_bucket_mode` and `LabelBucketStore::reserve_promote_bypass_to_bucket_mode`) before it writes the bucket-mode vertex row, releases the old bypass span, and bumps PMA segment counts.

The edge slab keeps `elem_capacity` exact while reserving one additional physical stable-memory page
when crossing a page boundary. This amortizes repeated `Memory::grow` calls during relocation-heavy
workloads without exposing the reserve as allocatable slots or changing free-span ownership. The
reserve is physical capacity only; failure-atomic tests must target logical allocation boundaries,
not an assumed exact page-growth event.

After the first commit write, no recoverable `Memory::grow` or allocation error remains. Physical capacity reserved during preflight is not canonical graph state: retaining it after an error is safe, and the pre-error logical layout reopens unchanged.

**Commit order invariant** (from `lara.rs`): relocate and rewrite all live pointers first; **only then** `release_span` old physical ranges. Queries never observe free-span slots as live adjacency.

**Labeled note:** per-vertex `release_vertex_edge_span_footprint` on routine growth is **not** this contract; see [ADR 0001](../adr/0001-labeled-segment-slide.md).

**Reopen integrity (composite + paired regions):** a composite store (`EdgeStore`, `LabelBucketStore`, `EdgeInlineValueStore`) and each graph that owns several of them (`LaraGraph`, `LabeledLaraGraph`) span stable-memory regions that must move together. On `init` the required regions are either **all empty** (create fresh) or **all populated** (reopen); a partially populated set is rejected (`*::InitError::PartialLayout`) instead of silently recreating and overwriting live regions or pairing an empty vertex column with live edge state. The check is applied at the graph-owned boundary too, so all subsystems go Fresh or Reopen together. The `FreeSpanStore` records header and its `free_span_by_start` index are a **paired** region: reopen rejects one-sided loss and re-runs `validate()` plus a `by_start.len() == active_count` check, so a stale or empty index cannot hide live spans and let the allocator hand out the same physical range twice. `FreeSpanStore::validate()` proves the bin↔index bijection by a **sorted merge**: it walks the size-class bins once collecting `(start_slot, id)` pairs, sorts them, then compares them against a single ascending sequential scan of `free_span_by_start` (via the paged map's forward `iter()`), advancing the index cursor at most `active + 1` times. This is `O(active)` reads plus an in-heap `O(active log active)` sort and avoids the per-record random index lookups the earlier check used; on the large reopen path (`bench_lara_free_span_store_reopen_*`) it roughly halves validation instructions at the cost of one transient `O(active)` pair buffer.

**Layout/version skew at the upgrade boundary:** every store header carries `magic` + `LAYOUT_VERSION` + `stride` (= `V::BYTES`), and `init` rejects a mismatch with a typed `InitError` (`BadMagic`, `IncompatibleVersion`, `StrideMismatch`) rather than decoding old-width rows as the new layout. This makes the header — not an ad-hoc schema-version cell — the single source of truth for on-disk row compatibility. A layout-changing upgrade shipped without a stable-memory migration is therefore caught at reopen, not as a silent misread. The graph canister forces this check at the upgrade boundary: `post_upgrade` calls `ensure_graph_initialized()` so a skew traps immediately with an actionable message (`graph stable layout is incompatible with this canister build (...); a stable-memory migration is required`), instead of lazily on the first post-upgrade query.

**Backing-memory-size guard at reopen:** after the magic/version/stride checks pass, the segmented overflow logs (`LogStore`, `PayloadLogStore`) and `FreeSpanStore` additionally verify that the backing memory is at least as large as the layout the header declares (`memory.size() * WASM_PAGE_SIZE >= required_bytes(header)`), returning a typed `InitError` (`OutOfMemory` / `InvalidLayout`) when it is not. These stores address per-segment slots at computed offsets (`HEADER_SIZE + leaf * segment_block_size + ...`); a truncated backing region or a corrupt `segment_count` would otherwise pass the header checks and only fail later as an opaque out-of-bounds trap on the first segment read. The guard turns that into an actionable reopen error.

**`value_blobs` asymmetry:** the payload blob map is excluded from the required-region set because a populated payload store with no wide-payload blobs legitimately leaves it empty. `EdgeInlineValueStore::init` still enforces the asymmetric rule: when the required regions are **Fresh**, `value_blobs` must also be empty (a surviving blob region alongside empty required regions is partial loss); when they are **Reopen**, `value_blobs` may be empty or populated.

**Best-fit completeness:** `take_best_fit` / `take_best_fit_whole` / `peek_best_fit` use a bounded per-bin scan to approximate best-fit cheaply, but must never report "no fit" while a fitting span exists in the start size-class bin. When the bounded scan finds nothing, the search continues over the remaining bin entries for the first fit, so allocation never forces an unnecessary slab/`elem_capacity` growth.

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

## Bidirectional mate contract (accepted, implementation planned)

[ADR 0048](../adr/0048-adaptive-lara-mate-index.md) places physical
entry-to-entry pairing in bidirectional LARA rather than a Graph facade B-tree.
Adjacency order and equal-neighbor occurrence rank remain authoritative. Small
or cold buckets resolve a mate by rank/select; selected PMA leaves may use a
packed, derived mate blob. Ordinary adjacency scans do not read this metadata.

The only fixed addition is a dedicated five-byte locator row per orientation
and leaf. It uses a custom fixed-row column modeled on `VertexStore`, rather
than enlarging `LabelBucket`, `LabeledVertex`, `SegmentEdgeCounts`, or
`SegmentSpanMeta`. Variable mate blobs use their own byte address space and a
mate-specific instance of the existing `FreeSpanStore` implementation; edge,
payload, and mate free ranges are never mixed.

Insertion returns exact physical locations by logical ordinal. Slot-preserving
rebalance requires no mate repair. Slot-renumbering compaction rebuilds affected
packed leaf blobs at the LARA boundary that publishes slot moves. This contract
is accepted but not yet implemented; the current Graph facade still uses
`EDGE_ALIASES`.

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

Payload slab ([labeled-edge-inline-values.md](./labeled-edge-inline-values.md)) follows the same logical compaction order as edge bytes; physical alignment with leaf rope is part of the labeled migration.

---

## Consensus checklist

Use this when reviewing LARA PRs:

- [ ] Scan paths do not touch `span_meta` or `FreeSpanStore`
- [ ] In-window rebalance does not `release_span`
- [ ] Segment relocate releases **one** retired leaf footprint after commit
- [ ] `FreeSpanStore` allocation is best-fit / coalesce, not scan-visible
- [ ] `FreeSpanStore` allocation never reports "no fit" while a fitting span exists (bounded scan has a first-fit fallback)
- [ ] `grow_segment_tree_to` and `promote_bypass_to_bucket_mode` reserve all fallible backing capacity before the first canonical write
- [ ] Composite/paired stable regions reopen all-or-nothing; partial layouts are rejected, not recreated
- [ ] Labeled changes do not deepen per-vertex tail-append + peel without ADR exception

---

## Related documents

- [lara-dgap-contract.md](./lara-dgap-contract.md) — DGAP mapping and labeled gap detail
- [adr/0001-labeled-segment-slide.md](../adr/0001-labeled-segment-slide.md) — labeled physical migration
- [adr/0045-unordered-batch-graph-mutations-and-lara-placement.md](../adr/0045-unordered-batch-graph-mutations-and-lara-placement.md) — **read-only planning implemented**; one-orientation batch commit implemented (`plan/reserve/commit` boundary, opaque graph-bound reservation, payload allocation with tail rollback, pre-write fingerprint/geometry validation, success and adversarial tests); new buckets, overflow-log batch writes, rebalance/relocation, dynamic expansion, and GraphStore orchestration remain planned
- [adr/0048-adaptive-lara-mate-index.md](../adr/0048-adaptive-lara-mate-index.md) — accepted physical-pairing and adaptive mate-index design; implementation planned
- [lara-labeled-migration-tests.md](./lara-labeled-migration-tests.md) — phase test gates (A–E)
- `crates/ic-stable-lara/README.md` — crate entry point
- `reference/DGAP/dgap/src/graph.h` — reference implementation
