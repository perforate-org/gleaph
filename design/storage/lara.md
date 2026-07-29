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

| Term           | Meaning                                                                                                    |
| -------------- | ---------------------------------------------------------------------------------------------------------- |
| **Localized**  | Rebalance and relocate work on a PMA window (leaf or ancestor window), not the whole graph on every insert |
| **Adjacency**  | CSR-style out-edge lists on a shared edge slab                                                             |
| **Relocation** | Physical edge bytes move; vertex rows are rewritten                                                        |
| **Array**      | Contiguous edge slab + explicit metadata stores                                                            |

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

| Event                                         | Retire to free span?                                     |
| --------------------------------------------- | -------------------------------------------------------- |
| Segment relocate / slide completes            | **Yes** — old `[physical_start, physical_start + total)` |
| Weighted rebalance inside fixed leaf capacity | **No** — slack stays inside the leaf assignment          |
| Per-vertex degree growth within CSR window    | **No** — append, tombstone reuse, or in-place pack       |
| Global `elem_capacity` growth                 | Optional tail; may also coalesce from free list          |

**Why LARA has this and DGAP does not (as explicitly):** DGAP often recovers space through `resize_V1` and implicit segment totals on a PMEM heap. LARA targets **incremental** relocation — `segment_slide`, in-place expansion into adjacent free gaps (`try_expand_segment_in_place`), and reuse without rewriting the entire slab on every cascade. The free-span store is the retirement pool that makes localized relocation safe and reusable.

**Failure-atomic stable mutations.** Two owner-level mutations are split into an infallible validation phase, a preflight phase that only grows backing memory, and a commit phase that publishes logical metadata:

1. `EdgeStore::grow_segment_tree_to` reserves `counts_store`, `span_meta`, and overflow-log capacity before it migrates counts, appends span-meta rows, resets new log indexes, and writes the edge header.
2. `LabeledLaraGraph::promote_bypass_to_bucket_mode` reserves bucket-slab and free-span capacity (via `LabelBucketStore::plan_promote_bypass_to_bucket_mode` and `LabelBucketStore::reserve_promote_bypass_to_bucket_mode`) before it writes the bucket-mode vertex row, releases the old bypass span, and bumps PMA segment counts.
3. `LabeledLaraGraph::reserve_one_orientation_batch` (Plans 0122–0124) validates the plan, reserves edge-slab `elem_capacity` and inline property bytes slab spans for clean-slab runs, and reserves per-leaf edge/inline-property-bytes overflow-log capacity for existing-bucket runs that do not fit the slab window, returning an opaque `BatchReservation` token before any canonical write. On failure it restores the logical edge capacity and inline property bytes occupied tail; inline property bytes already appended are retired to the inline property bytes free-list as reusable slack, and the underlying stable-memory pages are not shrunk. Overflow-log runs do not touch logical capacity or the inline property bytes tail before commit. `BatchReservation::rollback` consumes the token and applies the same restoration. `BatchReservation::commit` validates the token, graph instance, and bucket fingerprints before the first canonical byte write; after that, panic is an invariant violation and, in an ICP message, traps the whole message. Plans 0125 and 0128 extend the same boundary to pending-aware one-shot PMA leaf expansion: when neither the clean slab window nor the per-leaf overflow log can absorb the projected geometry (existing slab slots + existing overflow-log entries + pending batch edges), reserve expands the pinned leaf block in place by consuming an adjacent free span or growing the edge-store tail capacity, records the consumed free span for rollback, and commit rebalances the vertex span, folds preserved edge/inline-property-bytes overflow-log entries into the new slab layout, writes the pending edge/inline-property-bytes values, and publishes the leaf block growth in segment counts. Plan 0125 covers edge-only runs; Plan 0128 admits fixed, uniform non-zero inline property byte widths when the inline property bytes span is reusable or grows at the occupied tail, while rejecting non-tail relocation. GraphStore observes only the resulting admission classification and rollback boundary; PMA/log cursors and bucket heads remain LARA-owned.

The edge slab keeps `elem_capacity` exact while reserving one additional physical stable-memory page
when crossing a page boundary. This amortizes repeated `Memory::grow` calls during relocation-heavy
workloads without exposing the reserve as allocatable slots or changing free-span ownership. The
reserve is physical capacity only; failure-atomic tests must target logical allocation boundaries,
not an assumed exact page-growth event.

After the first commit write, no recoverable `Memory::grow` or allocation error remains. Physical capacity reserved during preflight is not canonical graph state: retaining it after an error is safe, and the pre-error logical layout reopens unchanged.

**Commit order invariant** (from `lara.rs`): relocate and rewrite all live pointers first; **only then** `release_span` old physical ranges. Queries never observe free-span slots as live adjacency.

**Labeled note:** per-vertex `release_vertex_edge_span_footprint` on routine growth is **not** this contract; see [ADR 0001](../adr/0001-labeled-segment-slide.md).

**Reopen integrity (composite + paired regions):** a composite store (`EdgeStore`, `LabelBucketStore`, `EdgeInlinePropertyBytesStore`) and each graph that owns several of them (`LaraGraph`, `LabeledLaraGraph`) span stable-memory regions that must move together. On `init` the required regions are either **all empty** (create fresh) or **all populated** (reopen); a partially populated set is rejected (`*::InitError::PartialLayout`) instead of silently recreating and overwriting live regions or pairing an empty vertex column with live edge state. The check is applied at the graph-owned boundary too, so all subsystems go Fresh or Reopen together. The `FreeSpanStore` records header and its `free_span_by_start` index are a **paired** region: reopen rejects one-sided loss and re-runs `validate()` plus a `by_start.len() == active_count` check, so a stale or empty index cannot hide live spans and let the allocator hand out the same physical range twice. `FreeSpanStore::validate()` proves the bin↔index bijection by a **sorted merge**: it walks the size-class bins once collecting `(start_slot, id)` pairs, sorts them, then compares them against a single ascending sequential scan of `free_span_by_start` (via the paged map's forward `iter()`), advancing the index cursor at most `active + 1` times. This is `O(active)` reads plus an in-heap `O(active log active)` sort and avoids the per-record random index lookups the earlier check used; on the large reopen path (`bench_lara_free_span_store_reopen_*`) it roughly halves validation instructions at the cost of one transient `O(active)` pair buffer.

**Layout/version skew at the upgrade boundary:** every store header carries `magic` + `LAYOUT_VERSION` + `stride` (= `V::BYTES`), and `init` rejects a mismatch with a typed `InitError` (`BadMagic`, `IncompatibleVersion`, `StrideMismatch`) rather than decoding old-width rows as the new layout. This makes the header — not an ad-hoc schema-version cell — the single source of truth for on-disk row compatibility. A layout-changing upgrade shipped without a stable-memory migration is therefore caught at reopen, not as a silent misread. The graph canister forces this check at the upgrade boundary: `post_upgrade` calls `ensure_graph_initialized()` so a skew traps immediately with an actionable message (`graph stable layout is incompatible with this canister build (...); a stable-memory migration is required`), instead of lazily on the first post-upgrade query.

**Backing-memory-size guard at reopen:** after the magic/version/stride checks pass, the segmented overflow logs (`LogStore`, `InlinePropertyBytesLogStore`) and `FreeSpanStore` additionally verify that the backing memory is at least as large as the layout the header declares (`memory.size() * WASM_PAGE_SIZE >= required_bytes(header)`), returning a typed `InitError` (`OutOfMemory` / `InvalidLayout`) when it is not. These stores address per-segment slots at computed offsets (`HEADER_SIZE + leaf * segment_block_size + ...`); a truncated backing region or a corrupt `segment_count` would otherwise pass the header checks and only fail later as an opaque out-of-bounds trap on the first segment read. The guard turns that into an actionable reopen error.

**`value_blobs` asymmetry:** the inline property bytes blob map is excluded from the required-region set because a populated inline property bytes store with no wide-inline-property-bytes blobs legiticounterpartly leaves it empty. `EdgeInlinePropertyBytesStore::init` still enforces the asymmetric rule: when the required regions are **Fresh**, `value_blobs` must also be empty (a surviving blob region alongside empty required regions is partial loss); when they are **Reopen**, `value_blobs` may be empty or populated.

**Best-fit completeness:** `take_best_fit` / `take_best_fit_whole` / `peek_best_fit` use a bounded per-bin scan to approxicounterpart best-fit cheaply, but must never report "no fit" while a fitting span exists in the start size-class bin. When the bounded scan finds nothing, the search continues over the remaining bin entries for the first fit, so allocation never forces an unnecessary slab/`elem_capacity` growth.

---

## LARA stores (edge slab side)

| Store                               | Contract                                | Scan?                     |
| ----------------------------------- | --------------------------------------- | ------------------------- |
| `EdgeStore`                         | Live edge bytes                         | Yes (via vertex row)      |
| `counts_store`                      | PMA `actual` / `total` per tree node    | No                        |
| `log`                               | Per-leaf overflow entries               | Yes (via `log_head` only) |
| `span_meta`                         | Leaf `physical_start` when order breaks | No                        |
| `free_spans` / `free_span_by_start` | Retired physical ranges                 | No                        |

---

## Bidirectional counterpart contract (current implementation)

Bidirectional LARA stores only the forward and reverse canonical adjacency projections. It does
not allocate a counterpart index, locator, blob, free-span store, or adaptive publication state.
`CounterpartScan` derives the equal-neighbor `PairOrdinal` from live logical bucket order and
selects the corresponding occurrence in the other projection. `canonical_handle` validates the
returned owner, label, orientation, target, and live relation before exposing it to Graph.

The paired owner coordinates canonical writes and deferred PMA maintenance. Reverse slot movement
is internal to LARA; Graph sidecars are updated from exact logical locations and never from a
persistent counterpart mapping. COUNTERPART-named storage, promotion, invalidation, rebuild, publication,
and measurement fixtures were removed because no adaptive algorithm has been accepted by ADR 0048.

A future ADR may investigate an adaptive accelerator only after measuring byte footprint, lookup
runtime, rebuild cost, and failure-atomicity against this exact scan. Such work is not part of the
current LARA contract and must not add a second source of edge identity.

## What is IC-specific (substrate only)

These are **implementation choices for Gleaph on canisters**, not part of the LARA algorithm definition:

- `ic-stable-structures::Memory` and stable memory region wiring
- Canister upgrade / persistence lifecycle
- `canbench` / Wasm benchmark harness

Changing substrate (e.g. host-side persistent mmap) should preserve the four contracts above.

---

## Labeled LARA (current alignment)

| Layer                                      | Status                                                                                                                          |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------- |
| Scan (`LabelBucket`, `LabelEdgeSpan`)      | Aligned with DGAP vertex + per-label windows                                                                                    |
| Overflow logs                              | Aligned (shared per-leaf log)                                                                                                   |
| Segment physical (rope) for **edge bytes** | **Implemented** — PMA leaf block per [ADR 0001](../adr/0001-labeled-segment-slide.md); per-vertex sub-ranges inside pinned leaf |
| Free-span usage for labeled edge bytes     | **Implemented** — segment footprint on leaf relocate; per-vertex peel only for unpinned legacy spans                            |

Inline property bytes slab ([labeled-edge-inline-properties.md](./labeled-edge-inline-properties.md)) follows the same logical compaction order as edge bytes; physical alignment with leaf rope is part of the labeled migration.

---

Plan 0158 evaluates a topology-aware policy: ScanOnly for low-degree/cold buckets, rank-indexed
only where measured byte and runtime gates pass, and compressed candidates only with proven
random-access invariants. Elias–Fano, restart-point delta coding, and shared-orientation maps are
measurement-only candidates; no caller activation follows from this plan.
The first candidate slice adds measurement-only restart-point signed delta sizing for arbitrary
rank sequences and monotone-only Elias–Fano sizing. Elias–Fano rejects non-monotone input rather
than reordering rank semantics; neither model is a production wire format.
On real fixtures, directed-high yielded 1,792 bytes for delta/restart and 1,800 bytes for the
conservative shared-orientation model, but only 3 of 128 sequences were monotone. Parallel-32
yielded 96 bytes for delta/restart, 84 bytes for shared orientation, and 32 bytes for Elias–Fano.
Elias–Fano therefore needs per-bucket fallback and cannot be a universal format; shared
orientation remains a measurement-only candidate.
The focused model probes measured 635.20K instructions for directed-high and 25.98K for
parallel-32, with zero heap and stable-memory page growth. These are candidate-evaluation costs,
not production lookup costs; runtime admission therefore remains a separate gate.
The restart candidate also has a measurement-only bounded reconstruction model: lookup starts at
the nearest restart and reconstructs at most `restart_interval - 1` deltas, with exact parity and
u32-boundary tests. It is not a serialized decoder or production format.
Focused restart reconstruction measured 8.59M instructions for directed-high and 690.02K for
parallel-32, with zero heap/stable-memory growth, versus decoded rank lookup at 1.56M and
323.90K. The restart candidate therefore fails the current runtime gate even where its logical
bytes are smaller.
The shared-orientation lookup model measured 672.22K instructions for directed-high and 175.43K
for parallel-32, with zero heap/stable-memory growth. It is below decoded rank lookup (1.56M and
323.90K) and its logical esticounterpart is smaller (1,800 versus 2,840 bytes; 84 versus 128 bytes), so
it is the only remaining compressed candidate passing the current measurement gates. It remains
measurement-only pending serialized locator, stale/malformed fallback, and production-layout
accounting.
The measurement candidate also round-trips through a strict temporary byte model with header,
ordering, width, truncation, trailing-byte, and rank-parity checks; it is not a stable wire format.
The decoded model matches canonical occurrence-rank counterparts for every identity in the
directed-high and parallel-32 fixtures and rejects out-of-range ranks.

Plan 0159 adds a measurement-only sampled paired-residual model. It stores block-local signed
`reverse_slot - forward_slot` values in 8/16-bit modes and falls back to raw counterpart slots. The
current logical esticounterparts are 2,696 bytes (21.06 B/edge) for directed-high and 60 bytes
(1.88 B/edge) for parallel-32 at block sizes 32/64. Higher-degree parallel fixtures show
checkpoint amortization (parallel-128: 276 -> 164 bytes; parallel-256: 532 -> 308 bytes for
B=8 -> 64), while directed-high is flat because its endpoint pairs are mostly single-rank groups.
Focused direct-offset lookup probes are 772.60K and 212.29K instructions respectively, with zero
heap/stable-memory growth. A bounded local-scan probe on parallel-32 measures 579.40K instructions
at B=8 and 1.26M at B=32/64, exposing the space/runtime trade-off. The temporary residual codec is
strict for residual and raw-fallback blocks; raw fallback carries an explicit reverse stream because
absolute counterpart slots cannot be derived by negating a residual. The model remains a candidate and is
not a production format.

Plan 0160 adds a measurement-only `SharedOrientation` policy candidate behind byte and runtime
gates. It is eligible only for dense, sufficiently requested buckets with exact/fail-closed
evidence; ScanOnly remains the fallback. This selector does not alter the production LARA layout
or persist a mode.

Plan 0160's initial common-fixture comparison reports shared-orientation at 1,800 bytes / 672.22K
instructions for directed-high and 84 bytes / 175.43K for parallel-32. Rank-indexed reports
2,840 / 1.56M and 128 / 323.90K; sampled residual reaches 60 bytes on parallel-32 but its
bounded local scan is 1.26M instructions. These are benchmark-only inputs to the threshold gate,
not production storage esticounterparts.

The undirected-high fixture reports 1,560 bytes for rank-indexed and treats shared-orientation as
unsupported because that model requires directed counterpart groups. Undirected adoption remains a
separate policy decision.

Plan 0161's initial orientation-free pair-rank accounting reports 1,544 logical bytes on
undirected-high versus 1,560 for rank-indexed. Pair-rank lookup measures 721.26K instructions,
versus 945.74K for rank-indexed and 16.83M for ScanOnly, with zero heap/stable-memory growth.
A synthetic 128-edge reordered exception pair measures 1,044 logical bytes and 118.07K lookup
instructions, also with zero heap/stable-memory growth; an explicit mismatch budget controls
whether such a pair is accepted. Persistent mutation maintenance and allocator costs remain
outside this measurement-only slice.

Plan 0162 measures a block-local permutation fallback for reordered undirected pairs. On a
synthetic 128-edge reversal, logical metadata is 212/180/164/156 bytes at block sizes 8/16/32/64,
while lookup is 188.72K instructions for each size with zero heap/stable-memory growth. This
remains a benchmark-only candidate and is not a stable LARA layout.

The topology synthesis is now explicit: low-degree/cold buckets, whether directed or undirected,
remain `ScanOnly`; dense directed buckets prefer the measured `SharedOrientation` candidate when its exact/fail-closed gates pass;
aligned undirected non-self buckets prefer pair-rank; reordered undirected pairs may use a bounded
block permutation exception; undirected self-loops require no counterpart metadata; and sparse-slot or
mixed-label buckets are evaluated independently. These are candidate precedences only. Shared,
pair-rank, and block-permutation formats still require mutation maintenance, stale/rebuild
handling, and stable-layout accounting before ordinary callers can activate them.

Plan 0181 adds an owner-facing physical-slot reader for slab and overflow-log locations. The current
Published bridge uses it only for singleton counterpart buckets; mixed-neighbor validation keeps a
full scan because exact equal-neighbor count and rank cannot be derived from bucket degree alone.
This is a transitional, crate-private implementation path, not the target query contract of ADR
0048/0050. The bridge remains until `counterpart.rs` migrates singleton validation to the logical read
surface; it must not become a Graph, Router, or graph-index identity API.

Plan 0163 confirms the self-loop shape boundary with isolated fixtures: directed self-loops have
two orientation rows, whereas undirected self-loops have one row and zero counterpart metadata. Plan
0164 adds a real isolated two-label fixture and a feature-gated physical slab/log location reader;
overflow-log indices are high-bit encoded only in measurement identities.

The measurement gate can encode directed self-loops through the directed rank adapter and confirms
that undirected self-loops carry no per-edge counterpart payload. The mixed-label fixture keeps both label
buckets independent, and sparse-slot uses real deletion-churn overflow-log locations rather than
logical live ordinals.

Synthetic topology probes measure sparse directed slots at 192 B ranked versus 148 B shared; the
corresponding lookup probes are 302.38K ranked and 171.32K shared instructions (ranked encoding
alone is 81.12K). The real mixed-label fixture remains separate at 52 B shared versus 96 B ranked
per label, with 350.59K shared versus 594.29K ranked instructions for a matched 4-edge-per-label
probe; its persisted ScanOnly counterpart is 15.94M instructions for 1,024 requests. The 190.77K/315.96K
pair is synthetic alternating-lookup evidence. The real sparse fixture measures 128 B ranked
versus 84 B shared and 300.35K versus 175.43K lookup instructions for 32 live edges per
orientation; its persisted ScanOnly counterpart is 45.59M for the same request count. These numbers are
candidate evidence only and do not permit cross-label sharing in the production layout.

Plan 0165 connects these real rows to the measurement-only adoption gate. Sparse slots and each
mixed-label bucket are evaluated independently: `SharedOrientation` is tried after the request,
degree, byte, and exactness gates; `RankedPacked` is the bounded fallback when its own gates pass;
otherwise the bucket remains `ScanOnly`. No metadata is shared across labels, and this precedence
does not activate an ordinary caller.

The cardinality policy is now explicit for the planned persistent layout. It counts live logical
edges per `(orientation, leaf, owner, label)` bucket, excluding tombstones and physical slot
capacity. The initial hysteresis defaults are `PROMOTE_MIN_LIVE_EDGES = 32` and
`DEMOTE_MAX_LIVE_EDGES = 16`. Below the promote floor a bucket is definitively `ScanOnly`; at or
above it, promotion still requires the exactness, stale, byte, and request/update amortization
gates. A published bucket is demoted at or below the demote floor or whenever a gate fails.
ScanOnly buckets are omitted from the blob directory, and a leaf with no indexed buckets allocates
no blob; the five-byte leaf locator remains the only derived row. These constants are policy
defaults to be confirmed by the adoption benchmark, not serialized wire fields. Plan 0172 now
implements this boundary in the measurement-only adoption gate and sparse footprint accounting;
production admission and codec changes remain deferred.

Plan 0166 adds deterministic measurement traces for one insert, delete, and reorder on the real
identity sets. Rebuilding three trace states costs 95.16K instructions for SharedOrientation versus
235.15K for RankedPacked on sparse slots, and 23.08K versus 43.90K for one mixed-label bucket.
Stale detection costs 468.95K instructions for sparse and 217.17K for mixed labels. The values are
charged only through an explicit read/update amortization helper; malformed or
cardinality-mismatched traces fail closed. Canonical LARA state and production stable layout are
untouched.

Plan 0167 charges the real canonical mutation boundary as well: sparse insert/delete/extract/rebuild
measure 72.74K / 162.18K / 126.71K / 144.90K instructions, while the mixed-label trace measures
60.96K / 35.70K / 26.19K / 18.25K. The 4,171 stable pages reported by these fixture runs are
setup allocation for measurement memory, not logical candidate size or production allocator
evidence. No counterpart metadata is published by this probe.

Plan 0168 applies the amortization gate: the sparse integrated cost is 506.53K instructions versus
45.59M for ScanOnly, and mixed-label is 141.10K versus 15.94M. Both SharedOrientation candidates
break even at one read per canonical update, subject to byte, exactness, stale, and fallback gates.
This remains measurement-only; no candidate is persisted or activated for ordinary callers.

Plan 0170 implements the first persistence slice as an isolated envelope codec and fixture-backed
stable map only; it does not allocate production counterpart regions or connect callers. The production
design keeps magic/version once in the region header, while locator metadata owns lifecycle,
candidate, epoch, identity, cardinality, offset, and total length; per-entry magic/version framing
is intentionally omitted.
The isolated fixture uses a fixed 32-byte region header matching existing LARA byte-store headers,
a 22-byte fixed locator value, and a separate raw payload region; it is not a production layout.
This bucket-locator/raw-payload split is not the selected production layout and does not allocate or
register production `MemoryId`s. Plan 0171 confirms the existing `CounterpartStorage` owner as the
production baseline: one five-byte locator row addresses one orientation/leaf and its blob
directory contains all indexed buckets for that leaf. Reopen and publication use the existing
four-region composite boundary. Plan 0175 now uses the compact blob
format at that owner: the 5-byte leaf locator remains unchanged, and the blob
header (`bucket_count` and `total_length`) and targets approxicounterpartly 15-byte directory entries
(owner 4, label 2, packed flags 1, cardinality 4, mapping offset 4), deriving mapping length from
the next offset/blob end. Its logical overhead target is `13 + 15B` bytes per leaf versus the
current `29 + 20B`, a saving of `16 + 5B` bytes before allocator and MemoryManager overhead.
Persisted bytes use this compact format exclusively. Plan 0173's
compact fixture measured 701 bytes for three indexed buckets versus 732 bytes for the historical
baseline, and 6,847 versus 7,571 instructions for encode/decode. The full crate canbench
sweep could not persist because the unrelated deferred undirected insert benchmark traps with
`CollectAllocationOverflow`; the focused compact/baseline probes passed.
Plan 0169 consolidates the persistence design: canonical LARA owns truth, while derived counterpart state
is one versioned record per orientation/leaf/owner/label bucket. Region metadata carries candidate
kind, lifecycle, topology identity, canonical epoch, cardinality, blob offset, and total length.
Proposed bounds are 65,535 entries and 2 MiB payload per bucket; overflow or malformed
records fail closed to `ScanOnly`. Mutation makes the derived record unavailable before canonical
commit, and publication occurs only after a complete rebuild validates the epoch, topology, and
candidate shape.
Interrupted or stale rebuilds are discarded or retried from canonical rows. This is a design
contract, not an implemented stable layout; future format changes use a fresh version/reset
boundary under ADR 0039. Checked locator ranges, canonical epoch, cardinality, and candidate-shape
validation reject malformed, stale, truncated, or oversized records before a published value is
returned.

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
- [adr/0045-unordered-batch-graph-mutations-and-lara-placement.md](../adr/0045-unordered-batch-graph-mutations-and-lara-placement.md) — **read-only planning implemented**; one-orientation batch commit implemented (`plan/reserve/commit/rollback` boundary, opaque graph-bound reservation token consumed on rollback, inline property bytes allocation with tail rollback and free-list slack, pre-write fingerprint/geometry validation, success and adversarial tests including allocator free-list shape); **GraphStore clean-slab orchestration implemented** (`try_insert_batch_edges_clean_slab` reserve-all-then-commit with explicit `Unsupported` fallback to the scalar path, cross-orientation reservation rollback on partial failure, directed/reverse/undirected/self-loop tests, scalar-vs-batch canbench); **per-leaf overflow-log batch append implemented** (`reserve_one_orientation_batch` admits existing-bucket runs to the shared per-leaf edge/inline-property-bytes overflow logs, reserve checks log and inline property bytes log capacity before any canonical write, commit appends entries in logical ordinal order and updates bucket heads/degree without changing stored_slots or vertex slab span, scalar fallback preserved for unsupported geometry); **Plans 0125/0128 pending-aware one-shot expansion implemented for existing-bucket runs** (one expansion per PMA leaf, adjacent free-span/tail growth, segment-count publication and rollback, preserved edge/inline-property-bytes-log fold, fixed-width inline property bytes span reuse or occupied-tail growth, edge and inline property bytes read-back/rollback coverage); **Plan 0129 internal physical-location results implemented** (LARA returns exact slab/overflow-log edge and inline property bytes locations keyed by ordinal and owner, GraphStore joins directed/reverse, undirected pair, and self-loop results without adjacency rediscovery); relocation, new buckets, persistent counterpart index, and public wire integration remain planned
- [adr/0048-lara-counterpart-resolution.md](../adr/0048-lara-counterpart-resolution.md) — accepted physical-pairing design; Plans 0132, 0142, and 0143 add one-pass live-slot traversal, exact scalar location consumption, canonical read-only leaf enumeration, and mutation invalidation/rebuild scheduling for supported named buckets. The Published Sampled/Packed lookup work described by the older plans is historical benchmark evidence only: it is superseded by ADR 0048's metadata-free `CounterpartScan` and is not an activation dependency or authoritative runtime contract. Alias removal is primarily a persistent-bytes optimization: the raw alias payload is 18 bytes per entry (excluding B-tree/allocator overhead), while MemoryManager page deltas are not per-edge measurements and ScanOnly instruction cost is only a guardrail. Plans 0147–0177 provide isolated AliasOnly/ScanOnly/rank-indexed fixtures, topology-specific selector logic, compact-only storage, and aggregate adoption status; these remain historical evidence and do not authorize ordinary-caller activation or alias removal
- The older ADR 0049 summary below predates the active public ordered endpoint and is retained only as historical context; the current status is recorded in ADR 0049 itself.
- [adr/0050-lara-traverse-read-api.md](../adr/0050-lara-traverse-read-api.md) — planned canonical logical-slot traversal and explicit inline-property read contract; activation is gated on the ADR 0048 counterpart replacement and full forward/reverse caller migration
- Plan 0180 reduces integrated Published request cost by reusing decoded blobs, validating packed ordering once, binary-searching packed mappings, and avoiding per-request bucket mapping clones. The full persisted sweep still measures the candidate slower than canonical scan for directed-high, parallel, and undirected-high, with zero heap/stable-memory deltas; all topologies remain Hold/ScanOnly and this is not an activation decision.
- [adr/0049-input-order-preserving-batch-graph-mutations.md](../adr/0049-input-order-preserving-batch-graph-mutations.md) — **partially implemented**; retains ADR 0045's implemented placement/reservation/commit substrate and the active input-order-preserving edge API. The Router mixed public shape, Graph-owned immutable mixed envelope, Graph journal identity/aggregate receipt wire shapes, and pure phase planner are defined, including request-local new-vertex ordinals and a separate mixed fingerprint domain. Mixed canonical phase execution, vertex allocation-table publication, journal admission, and Router dispatch remain planned; LARA does not own the mixed request or its two-phase failure contract.
- [lara-labeled-migration-tests.md](./lara-labeled-migration-tests.md) — phase test gates (A–E)
- `crates/ic-stable-lara/README.md` — crate entry point
- `reference/DGAP/dgap/src/graph.h` — reference implementation
