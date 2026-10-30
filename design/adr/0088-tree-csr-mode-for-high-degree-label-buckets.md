# 0088. Tree-CSR mode for high-degree label buckets (LTB block store)

Date: 2026-08-29
Status: accepted with amendments (raw-block validated, 2026-08-30; Gate 3
pass after Plan 0316 block-batched writes; Gate 4 passes; Gate 2
records per-row amend verdicts with full_scan achieving ~41 ins/edge at
both 4K and 65K). Gate 3 edge-only: 152.82K ins (vs 9M target = 59× under
target, vs Plan 0315's 52.44M = 343× improvement); w=32: 1.16M ins (vs
30M target = 26× under target, vs Plan 0315's 105.39M = 91×
improvement). The full-canister implementation slice (Plan 0317) is now
unblocked; the remaining Gate 2 rows are amend (deterministic splitmix
closure and per-write payload constant in non-promotion ops) and
recorded as design-scope evidence, not as blockers.)
Last revised: 2026-09-22

Implementation status: **Partially Implemented**. Native property-tree traversal, bounded
point-state/no-op admission, selected tree-property hydration and slab-compaction exclusion are
implemented. They do not establish live property-tree deletion, matching mutation or demotion
safety. See the [deletion prerequisite](#property-bearing-deletion-prerequisite-2026-09-22).

### Current benchmark registrations

[Plan 0401](../../plans/0401-canbench-export-retirement.md) retires the six
`tcsr_{4096,65536}_{full_scan_descending,random_ordinal_access,insert_grow}`
exports that measured the bench-only `TreeCsrBucket` scaffold. Their gate tables below
remain historical design evidence, not fresh production Graph measurements. Production
Graph sweeps, the two scaffold promotion probes and LTB reopen measurements remain
registered with unchanged fixtures and assertions. This retirement changes neither the
layout/math implementation nor native tests or production performance requirements.
Persisted benchmark results are preserved historical measurements; a complete current
registry artifact requires a separately allocated unfiltered run.

Supersedes the deferred Stage 2 direction of
[ADR 0022](0022-degree-driven-hub-edge-storage.md): the dedicated-span tier (2a)
is subsumed by this design's bounded-footprint invariants, and the B-tree tier
(2b) remains rejected on ADR 0022's own measured evidence. Scope is **Labeled
LARA**; normal (unlabeled) LARA adopts the same contract later as a second
instance (planned, not part of this ADR's implementation).

## Context

A labeled LARA bucket `(vertex, label, orientation)` stores its adjacency as one
contiguous span inside a PMA leaf on the CSR edge slab
([lara.md](../storage/lara.md)). Every physical policy — leaf relocation,
weighted rebalance, occupancy-aware placement, free-span retirement, batch
reservation — operates on that span. Nothing bounds the span's width: a
high-degree bucket (a "super-bucket": one celebrity's `FOLLOWED_BY`, a hub's
reverse adjacency) grows its leaf footprint linearly with degree.

ADR 0022 already fixed the *delete-path* symptoms of high degree (O(D²) → O(D)
stepped purge, bounded overflow-log chains) and rejected a B-tree hub tier on
measured evidence: B-tree scans cost ~2,004 ins/edge versus ~318 ins/edge on the
contiguous slab (6.3×), inserts were 3.6–8.1× slower, and IC economics (queries
free, updates metered) favor the slab. Its closing recommendation was to look
for a structure that keeps contiguous-slab reads while bounding mutation and
placement costs ("a slab 'target column' plus a small free/index map", ADR 0022
value-layout verdict).

Gleaph is pre-production: the repo policy requires one canonical layout, fresh
state when the layout changes, and no compatibility branches or migrations.

## Problem

Three costs remain unbounded in the current layout, all proportional to the
degree of the largest bucket in a leaf:

1. **Physical relocation unit.** Segment relocate/slide and weighted rebalance
   copy the whole leaf block; one 1M-edge bucket makes every such operation a
   multi-MiB copy, repeated by density cascades.
2. **Contiguous-span allocation.** Growth of a large bucket demands one
   contiguous span of `stored_slots` (edge) and `stored_slots × width`
   (inline-property bytes). Giant requests defeat `FreeSpanStore` best-fit,
   fragment the slab, and force `elem_capacity` tail growth.
3. **Leaf geometry reasoning.** Per-bucket spans may exceed the per-vertex
   quota without bound (the oversized-span regime behind the data-loss bug
   fixed in ADR 0022), so worst-case placement and slack reasoning never
   closes.

Measured problem sizing at high degree (beyond ADR 0022's 256-edge growth-
contention probe) is an explicit acceptance gate of this ADR, not a
precondition for recording the design.

## Existing Architecture Assessment

- The **overflow log** buffers window-full inserts but is capped at 170 entries
  per leaf segment and folds back into the same contiguous span; it cannot
  bound the span itself.
- **Pending-aware one-shot expansion** (Plans 0125/0128) and **segment
  slide/relocate** move or grow spans efficiently but still move O(degree)
  bytes per event.
- **`promote_bypass_to_bucket_mode`** proves the mode-transition pattern
  (preflight/reserve/commit, fingerprint-validated publish) but only moves a
  vertex from bypass rows to bucket descriptors; the bucket's span stays
  contiguous.
- ADR 0022's B-tree prototype showed that replacing the slab with a keyed map
  trades the scan path away. No existing concept bounds the leaf footprint of
  one bucket while keeping raw 4-byte contiguous reads.

The missing concept is an **indirection layer whose unit is a fixed-size block
of raw edge rows**: the CSR keeps a small root of block addresses, and edge
bytes live in blocks that never participate in leaf relocation.

## Alternatives

### A. B-tree tier (ADR 0022 Stage 2b)

Rejected — retained verdict. Measured 6.3× scan regression (node traversal +
key/value deserialization), 3.6–8.1× insert regression, higher metered cost on
every mutation. Not revisited.

### B. Dedicated contiguous span pin (ADR 0022 Stage 2a)

Evacuate the hot bucket to its own PMA-external contiguous span. Minimal
implementation and a pure contiguous scan, but growth still copies O(degree)
bytes (amortized doubling), giant contiguous allocations still fragment the
free-span store, and the measured benefit was modest (~6.5K ins/insert of
recovered rebalance contention). Solves half the problem; subsumed by D.

### C. Chained blocks (linked list of blocks)

Simpler than a tree, but ordinal random access degrades to O(degree/B),
degrading counterpart resolution and ordered reads. Rejected.

### D. Tree-CSR mode (chosen)

For a bucket past a promotion threshold, the CSR span stores **block
addresses** (a root array); edge rows live in fixed-size blocks in a dedicated
block store. Bounded relocation unit (roots only), no giant contiguous
allocations (fixed 4-KiB blocks, trivial free list), block-internal reads stay
raw contiguous 4-byte rows. Precedented by filesystem indirect blocks and
dynamic-graph stores (Sortledton unrolled skip lists, Teseo fat trees).

**Terminology.** "Tree-CSR" names an **ordinal-addressed indirect block tree**
(fixed fanout, left-packed), not a B-tree: interior entries carry no separator
keys, lookup input is a logical position (shift/mask per level), and no
target-keyed search exists anywhere in the structure. ADR 0022's measured
B-tree costs (node traversal + key/value deserialization, ~2,004 ins/edge) do
not apply to block-internal reads here, which are the same raw 4-byte rows as
the slab. Reviews and benchmarks must not conflate the two.

Sub-decisions resolved within D:

- **Addressing: bare `u32` block ids, root array as the only mapping.** A keyed
  map (`(vertex, label, ordinal) → block`) would add a second owner of bucket
  geometry, O(log n) lookups on the scan path, and B-tree node overhead;
  rejected. Root slots in tree mode are raw u32 block ids (no 30-bit
  `VertexRef` geometry, no tombstone bit — the root is always dense).
- **Root lives in the CSR span, not in an LTB block.** A root-in-block variant
  would waste most of a 4-KiB block at small tree sizes, add +1 indirection to
  every access, and hide the bucket's weight from PMA density math. Rejected.
  (The weight at stake is the root region's *span residency*, which stays
  resident and mode-blind. Tree **edge rows** never count toward leaf `actual`
  in either variant: `actual` counts live edge records that occupy edge-slab
  slots — ADR 0096 §5.)

## Decision

### Decision (2026-08-29)

Initial acceptance with amendments recorded by [Plan 0313](../../plans/0313-adr0088-tree-csr-measurement-gates.md). Gate verdicts were recorded as `defer` because the prototype used a `StableBTreeMap<BlockId, Vec<u8>>` scaffold whose `O(log n)` block-write cost dominated every measurement; the design's amortization claim was plausible but unproven.

### Decision (2026-08-30) — Plan 0315: raw-block LTB backend

The prototype's block storage was replaced with a raw-block `LtbRawBlockStore<M>` that talks directly to stable memory (`memory.read` / `memory.write` against fixed-stride 4 KiB blocks). Gate verdicts after [Plan 0315](../../plans/0315-adr0088-raw-block-ltb-revalidation.md):

| Gate | Verdict | Notes |
|------|---------|-------|
| Gate 1 (problem sizing) | amend | Slab 4K insert_grow = 28.74M, 65K = 424.41M. 1M anchored by cargo-test host-wall-clock. |
| Gate 2 (parity matrix)   | amend | full_scan at 4K = 14,540 ins/edge (down from 41,540 at 1024-edge baseline); random_ordinal / insert_grow at 4K comparable to slab baseline within 1.5×–6×. |
| Gate 3 (promotion cost)  | reject | Edge-only = 52.44M ins, w=32 = 105.39M. Per-write payload constant (~13K ins/edge in `write_payload`) is the remaining fixed cost. |
| Gate 4 (LTB reopen)      | pass | `ltb_reopen_4096` = 1.30M ins for 4096-block envelope = **~317 ins/block**, 8.9× cheaper than `bench_r_fs_ro_4096` precedent (~2,830 ins/block). |

Plan 0315 closed Gate 4 and moved Gates 1/2 to amend, but Gate 3's per-write constant required a follow-up slice.

### Decision (2026-08-30) — Plan 0316: per-write payload constant fix

The `promote_from_slice` and `promote_from_slices_with_property` commit phases were rewritten from per-slot read-modify-write (1024× I/O amplification) to block-batched writes (one `Memory::write(4096)` per block). A new `LtbRawBlockStore::write_payload_partial(id, offset, src)` API was added for sub-block writes. Gate 3 verdicts after [Plan 0316](../../plans/0316-adr0088-per-write-payload-constant-fix.md):

| Bench | Plan 0313 (StableBTreeMap) | Plan 0315 (raw-block) | **Plan 0316 (block-batched)** | Target | Improvement |
|-------|---------------------------:|----------------------:|-----------------------------:|-------:|------------:|
| `tcsr_promote_edge_only` | 315.74 M | 52.44 M | **152.82 K** | ≤ 9 M | **2,066× vs scaffold**, **59× margin** |
| `tcsr_promote_inline_property_w32` | 987.88 M | 105.39 M | **1.16 M** | ≤ 30 M | **851× vs scaffold**, **26× margin** |

The I/O amplification collapse from `O(S)` per slot (4 KiB read + 4 KiB write per slot) to `O(B)` per block (one 4 KiB write per block) confirmed the design's amortization claim (`O(T_promote × (4 + w))` bytes, not `O(T_promote)`).

### Decision (2026-08-30) — Plan 0322: partial read and chunk iter

The read side was brought to parity with the write side per [Plan 0316 §Notes](../../plans/0316-adr0088-per-write-payload-constant-fix.md):

- `LtbRawBlockStore::read_payload_partial(id, offset, &mut [u8])` — sub-block read for point lookups and conditional reads.
- `LtbRawBlockStore::for_each_chunk<F>(&self, f)` — chunk-buffer iterator that yields `(start_slot, &[u8])` slices, mirroring the CSR-slab leaf-chunk-buffer pattern. Callers decode `u32::from_le_bytes(slice[i..i+4])` directly with no per-slot stack allocation.
- `TreeCsrBucket::range_target(slot)` — point lookup consuming `read_payload_partial`; `random_ordinal_access` refactored to call `range_target` (no public API change).

No new canbench benches added; wasm export-name budget preserved at **16,776 chars / 20,000 limit** (3,224 chars of structural headroom from [Plan 0314](../../plans/0314-canbench-name-rationalization.md)).

### Final verdict

| Gate | Verdict | Status |
|------|---------|--------|
| Gate 1 (problem sizing) | amend | slab 4K / 65K measured, 1M cargo-test host-wall-clock anchored |
| Gate 2 (parity matrix)   | amend | full_scan collapse from 14,540 → 41 ins/edge at 4K = 351×; random_ordinal / insert_grow / delete_half within target bounds or documented as prototype-only |
| Gate 3 (promotion cost)  | pass  | edge-only = 152.82 K (59× under single-digit M target); w=32 = 1.16 M (26× under 30 M target) |
| Gate 4 (LTB reopen)      | pass  | 317 ins/block, 8.9× cheaper than FreeSpanStore precedent |

The Tree-CSR design is **validated against a faithful LTB backend**: block-internal reads match CSR-slab scan locality (chunk-buffer iter, amortized O(1) per slot), and promotion cost is bounded by `O(T_promote × (4 + w))` bytes (Plan 0316 verified at 152.82 K ins for edge-only 4096-edge promotion).

### Decision (2026-08-30) — Plan 0318 amendments: cap semantics correction

After Plan 0318 Step 4 (commit `44c82d3b2`) implementation, a unit confusion in
the cap wording introduced by the [Plan 0317 amend](../plans/0317-adr0088-tree-csr-implementation.md)
was identified and corrected. The wire truth of the cap constants is:

- `R_max = 1024` is the **root-array fan-out cap** (the dense `u32` block_id
  array that forms the root region of a tree bucket). It bounds the size of
  one root, *not* the logical-slot count of a tree bucket.
- `T_promote = 1024` is the **slab → tree promotion threshold** (the
  `alloc_space` size that triggers promotion into tree mode) and the slab
  mode cap on `alloc_space = stored_slots + alloc_gap`. Re-tuned from 4,096 on
  2026-09-20; see "Threshold re-tune".

The logical-slot capacity of a tree bucket is **`coverage_at_depth(MAX_DEPTH)
= 2^30`** slots (per §4: depth 1 ≤ 2^20, depth 2 ≤ 2^30, depth 3 ≤ 2^40; the
fail-closed `MAX_DEPTH = 3` bounds the practical capacity to 2^30 distinct
edge slots under the current 1024-slot blocks and 1024-entry root fan-out).

**Plan 0325 update (right-spine cascade)**: the cap was
**previously 2^20** (interim `TreeRootCapacityReached` guard,
Plan 0318 §Step 7 amend). Plan 0325 shipped the right-spine
cascade and the cap is now **`TREE_STRUCTURAL_CAP = 2^30`** —
the exact depth-2 coverage. The cascade grows depth 1 → 2 at
stored = 2^20 (right-spine interior mint) and fail-closes at
stored = 2^30 + 1 (depth 2 + root_len = R_MAX, `MAX_DEPTH` not
yet reached but the structural cap binds first). Lifting the cap
to 2^40 (depth 3 in production) would require an ADR amend; out
of scope for Plan 0325.
The 1024 root entries per level mean that a `root_len` crossing `R_max`
triggers **deepen** (Step 7), not a slot-cap reject. The text below
"Tree bucket span (no gap) ≤ 2 × R_max = 2,048 slots (8 KiB)" in §3 is the
size of *one root + one interior level* (i.e. the storage cost of the second
deepen step), not a logical-slot cap — the previous wording risked being
misread as a slot cap.

The "R_MAX = 1024 slots (4 KiB)" wording in [Plan 0317 §Step 3.5](../plans/0317-adr0088-tree-csr-implementation.md)
is a **unit error**: 1024 slots × 4 bytes = 4 KiB is the dimension of a
single LTB block (one data block holds 1024 edge slots), not a slot cap on a
tree bucket. Plan 0317 has been amended with a correction note; the
production code in [Plan 0318](../plans/0318-tree-csr-implementation.md)
`check_alloc_cap` / `cap_for_mode` uses `TREE_STRUCTURAL_CAP = 2^30` for
tree-mode slot checks (the `MAX_DEPTH` fail-closed boundary) and
`R_max` only as the root-array fan-out cap that the deepen's `root_len ≤
R_max` check uses.

Under the current placeholder `alloc_gap = T_promote - stored_slots` (Plan
0317 §3.5 placeholder, weighted gap deferred to a later slice),
`compute_bucket_allocation(slab) ≡ T_promote` for any `stored_slots` in
[0, T_promote]. The promote trigger therefore uses `stored_slots ≥
T_PROMOTE` directly; when the weighted gap is introduced the trigger
switches to `alloc_space ≥ T_PROMOTE` and `compute_bucket_allocation` becomes
strictly < `T_PROMOTE` until the cap is reached.

Implementation slice (Plan 0317) is unblocked from the cost side. The full-canister wire-up of `LtbRawBlockStore<VirtualMemory>` per orientation (forward + reverse) into `LabeledLaraGraph` is the next step. The implementation slice must re-run Gate 2 against the production `VirtualMemory` backend (numbers above are on the `VectorMemory` test backend; production `Memory::write` may differ in constant cost by ±20%) and run a PocketIC-backed 1M-degree sweep to close Gate 1's deferred row.

Per Plan 0316 §Notes and Plan 0322 §Notes, the following remain explicitly deferred to later slices:

- [Plan 0318](./0318-tree-csr-demotion-tree-to-slab.md) — Demotion (tree → slab), a benchmark-gated maintenance operation.
- Plan 0319 — `materialize_inline_property_stream` migration primitive (currently recorded as planned).
- Plan 0320 — Batch admission widening to tree-mode buckets.
- Plan 0317 — Tree-CSR implementation in `LabeledLaraGraph`. Wires `LtbRawBlockStore<VirtualMemory, E>` per orientation (forward + reverse), adds the descriptor mode-flag bit, promotion / deepen / flatten transitions, mode dispatch in the bucket access constructor. Plan 0322 must close before Plan 0317 starts so the production code paths can use `range_target` and `for_each_chunk` from day one.
- Plan 0321 — Normal LARA (unlabeled) tree mode as a second instance of the same contract.

### 1. LTB block store

One new store per orientation (forward, reverse): the **LARA Tree Block (LTB)
store**, a single stable-memory region (one `MemoryId` each; ADR 0007 inventory
update on implementation) holding a 64-byte header plus fixed-stride blocks.

```text
-------------------------------------------------- <- Address 0
Magic "LTB"                            ↕ 3 bytes
Layout version (=1)                    ↕ 1 byte
Payload bytes per block (1024 × E::BYTES) ↕ 4 bytes  (wire truth; 4096 for 4-byte edges)
Root fan-out cap R_max (=1024)         ↕ 4 bytes    (wire truth; see §4)
Block capacity (allocated block slots) ↕ 8 bytes
Tail next (first never-minted id)      ↕ 8 bytes
Free head (intrusive list; MAX=none)   ↕ 4 bytes
Free count                             ↕ 4 bytes
Reserved (must be zero)                ↕ 28 bytes
-------------------------------------------------- <- Address 64 (HEADER_SIZE)
Block 0: block header ↕ 16 B | payload ↕ 1024 × E::BYTES (stride 4112 for 4-byte edges)
Block 1: ...
-------------------------------------------------- <- 64 + capacity × 4112
```

Per-block header (16 bytes): `kind` (1 B: 0 Free, 1 Edge, 2 InlineProperty,
3 EdgeInterior, 4 InlinePropertyInterior), reserved (1 B, zero), bucket label
key wire (2 B), owner vertex id **or next-free id when `kind == Free`** (4 B),
stream ordinal (4 B), level (1 B, validation metadata only), reserved (3 B,
zero). Kind/owner/ordinal/level are **validation metadata**, never addressing
keys; the root array is the single source of reachability.

**Block id domain:** bare u32 with `NULL_BLOCK = u32::MAX` as the free-list
terminator (effective domain 2³² − 1 blocks). Shared capacity across all
buckets is `payload × (2³² − 1)` ≈ 16 TiB of edge rows — not the narrowest
bound: physical canister stable memory (order of hundreds of GiB; verify the
current platform limit at implementation) and the existing 36-bit slab slot
domain both bind one or more orders of magnitude earlier. Fail-closed: minting
past the domain is a typed error before any canonical write. Widening trigger:
raise `payload_bytes` (capacity scales linearly); an 8-byte root entry is a new
layout ADR under the fresh-state policy.

**Blocks never relocate.** PMA/rope operations move root arrays only; a block's
id is stable from mint to release. Allocation pops the intrusive free list,
else mints at `tail_next`. Release pushes the id back with `kind = Free`.

**Memory-manager wiring (ADR 0043):** the LTB regions start at a **64-page
(4 MiB) bucket policy** per orientation — the property-store tier. Rationale:
the LTB aggregates **both** streams (`(4 + w)` bytes per tree edge), is the
largest expected region in hub-heavy shards, and its 64-page per-region
ceiling (256 GiB under the shared extent budget, 256 extents/GiB) matches the
CSR slab's own 36-bit addressing scale — a 32-page policy would cap the
spillover target (128 GiB) below the slab domain it relieves. Experimental
value, confirmed by the acceptance-gate footprint measurements.

> **Superseded by code (Plan 0338 reconciliation):** the wired
> `GRAPH_MEMORY_MANAGER_POLICIES` carries `(FWD_LTB, 16)` / `(REV_LTB, 16)` —
> a **16-page extent allocation granularity**, not a 64-page capacity cap. The
> policy value is the per-memory extent granularity in pages (the global
> `MAX_EXTENTS = 65,536` budget, not the policy value, bounds capacity); see
> `design/storage/stable-memory-inventory.md` for the authoritative record.
The store is **lazily created**: the region stays physically unallocated
(size 0) until the first promotion, so graphs with no tree buckets pay
nothing (the §8 asymmetric reopen rule already admits the empty region).
Because the VMM bucket quantum amortizes `Memory::grow`, the LTB store adds
no page-reserve amortization of its own.

### 2. Tree mode of a label bucket

A `LabelBucket` descriptor gains **one flag bit** in the existing reserved bits
of its packed word; the descriptor stays 29 bytes. In tree mode:

- `edge_start` = start of the **root region** in the vertex edge span:
  `[edge root | inline-property root]`, each a dense array of u32 block ids.
- `stored_slots` = tombstone-inclusive logical slot count (truth for both
  streams).
- `degree`, `inline_property_byte_width` keep their meanings.
- `inline_property_bytes_offset = 0`, `inline_property_bytes_slab_slots = 0`
  (invariants — the byte-slab span is not used).
- Overflow-log fields stay live but hold **root (block-id) entries only**
  (§6).

All geometry is derived — no new persisted field:

```text
B       = 1024 edge slots per block      (EDGES_PER_BLOCK)
payload = B * E::BYTES                  (4096 bytes for the 4-byte production edge)
K       = floor(payload / w) values per property block (w > 0)
edge root len     = ceil(S / B^d)        d = derived depth (§4)
property root len = ceil(S / (K·B^(d'−1)))
span width        = edge root len + property root len      (gap = 0, §3)
```

**Edge width:** tree mode stores one `E::BYTES`-wide edge record per LTB slot,
so a block holds `EDGES_PER_BLOCK = 1024` edges at every width and the payload
is `1024 × E::BYTES`. For the 4-byte production edge this is exactly the
original 4096-byte payload and 4112-byte stride, so production layout, capacity
and measurements are unchanged. The store is typed by the edge record
(`LtbRawBlockStore<M, E>`), writes `1024 × E::BYTES` into the header, and
reopen rejects a store opened with a different width (`InitError::
BadPayloadBytes`), so a mismatched geometry fails at open instead of
mis-striding blocks.

Every edge width promotes. The former `E::BYTES == 4` restriction and its slab
carve-out — added after the canbench `bench_l_s2_det_sat_4096` bench trapped on
the tree-append guard following a silent mis-transcription — are removed: tree
mode addresses every slot by `E::BYTES`, and both the slab root region and the
interior block payloads are indexed in edge-record slots (one `u32` block id in
the slot's first four bytes, zero-padded), so one raw byte copy still moves a
whole level and a wider record only scales the block. The insert dispatcher's
run-full exit is tree promotion for every width, and the retired
`TreeModeEdgeWidthUnsupported` errors are gone.

Logical positions are unchanged: `BucketEntryPosition` remains the bucket-local
tombstone-inclusive position (`crates/ic-stable-lara/src/traverse.rs`), so
`EdgeHandle`, sidecar keys, ADR 0052 compaction semantics, and
`CounterpartScan` `PairOrdinal` derivation are untouched. Physical resolution
is pure arithmetic (`i → (root[i / B], i % B)` at depth 1, one shift/mask pair
per level), plus the overflow-log suffix rule of §6. The Plan 0129
physical-location surface gains a third variant: tree block `(block_id,
in-block offset)` alongside slab window and overflow log.

**Packing invariant:** every non-tail block (and, per level, every non-spine
interior block) stores exactly its full slot count; only the right-spine tail
is partial. Deepen and appends fill left-to-right — there is no B-tree-style
balanced split — because the O(1) position arithmetic depends on left-packed
fullness. Roots are always dense — tombstones exist only inside edge blocks.

Mixed-mode counterpart pairs (forward slab / reverse tree, etc.) are supported
with no coupling constraint; counterpart logic operates on logical positions
only. Test matrix: slab×slab, slab×tree, tree×tree.

### 3. Zero-gap spans and bounded leaf footprint

Tree-mode spans carry **no slack**: span width equals the derived root length
exactly. Root growth (+1 slot per B inserts, per K for the property root) uses
the ordinary vertex-local update contract; amortized cost is ≤ leaf-copy /
1024 per insert. The rope/PMA/placement layers treat the root span exactly
like a small slab bucket (a 4-entry root is physically a 4-slot span) and stay
**mode-blind** — the tree flag is read at exactly one dispatch point, the
bucket access constructor. A mode branch in rope/PMA/placement code is a
review rejection. Root occupancy is therefore real PMA geometry (`total`,
`bucket_physical_resident_slots`), while the density **numerator** (`actual`)
counts only live edge records that occupy edge-slab slots: tree edge rows live
in LTB blocks and contribute no slab slot (ADR 0096 §5).

Capacity bounds (all fail-closed at the allocation site):

| Region                                   | Bound                             | Enforcement                                  |
| ---------------------------------------- | --------------------------------- | -------------------------------------------- |
| Slab bucket edge span (incl. slack)       | `T_promote` = 1,024 slots (4 KiB) | growth clamp + crossing compact-or-promote   |
| Slab bucket inline-property span          | `T_promote × w` bytes             | follows `stored_slots`                       |
| Tree bucket span (no gap)                 | root entries ≤ `R_max` per level; logical slots ≤ `coverage_at_depth(MAX_DEPTH) = 2^30` | gap-0 invariant + deepen (when `root_len > R_max`) |
| Per vertex                                | above × `MAX_VERTEX_LABEL_BUCKETS` | existing bucket-count cap                    |

Every per-bucket leaf region is thus constant-bounded, which: caps the
relocation/rebalance unit, removes giant contiguous allocations from both the
edge slab and the inline-property byte slab (retired spans become small and
bin-friendly), and closes the oversized-span placement regime. This subsumes
the remaining (fragmentation/locality) motivation of ADR 0022's dedicated-span
tier. PMA leaf-level gaps (density invariant) and tombstone slack awaiting
compaction remain and are owned by their existing contracts.

### 4. Depth: derived, uniform, bounded

Depth is **not persisted**. It is a pure function of `stored_slots`:

```text
depth(S) = min { d ≥ 1 : ceil(S / B^d) ≤ R_max }        (edge stream)
           min { d ≥ 1 : ceil(S / (K·B^(d−1))) ≤ R_max } (property stream)
```

The two streams derive independently and may differ. A persisted depth field
could contradict `stored_slots`; derivation makes that state unrepresentable.
Natural hysteresis: `stored_slots` is tombstone-inclusive and **must not
shrink outside compaction/maintenance** — deletes only tombstone; tail-trim on
the delete path is prohibited in tree mode (a trim crossing a depth boundary
would force a flatten inside a delete). Coverage (B = R_max = 1024): depth 1 ≤
2²⁰ slots, depth 2 ≤ 2³⁰ (the scale of the 30-bit distinct-target payload
domain; parallel edges may exceed it), depth 3 ≤ 2⁴⁰.

`MAX_DEPTH = 3` is a fail-closed **structural** boundary of the current
encoding, not a semantic degree limit. Tree-mode capacity is deliberately
derived from `B`, `R_max`, and `MAX_DEPTH` with **no artificial per-bucket
size cap**: the point of tree mode is to switch a large adjacency to a dense
representation, not to reject it (`T_promote` is only the entry threshold, not
a maximum). An insert whose derived depth would exceed `MAX_DEPTH` fails with
a typed error before any canonical write; supporting deeper trees or a larger
payload is a future layout ADR under the fresh-state policy. In practice,
physical stable memory binds well before the depth-3 coverage.

**Deepen** (the insert that pushes the derived root length past `R_max`):
reserve mints the new data block plus `ceil(new_root_len / B)` interior blocks
(`kind = *Interior`); commit copies the current root ids (span + unfolded log
entries) into the interior blocks, rewrites the span to the interior ids
(right-spine partial allowed), writes the pending edge, publishes the
descriptor — one atomic descriptor write flips both `stored_slots` and layout,
so derived depth and physical layout can never disagree in a published state.
Existing blocks do not move; total cost O(R_max) = one 4-KiB copy. **Flatten**
is the inverse, only ever inside compaction/maintenance; interior blocks are
released after publish (commit-order invariant). The operation is
level-generic (2→3 identical one level up).

### 5. Inline-property bytes

Same block store, `kind = InlineProperty`, same 4,096-byte payload. Property
position `i` ↔ edge position `i` (1:1, tombstone-inclusive); per-block residue
`payload mod w < w` is dead bytes. The property root follows the edge root in
the span; the +1 shift when the edge root grows is absorbed by fold/rebalance
(§6), not paid on the hot path.

**NOTE (Plan 0320 §F-2, 2026-09-01)**: the tombstone-inclusive 1:1
mapping above describes the **tree** geometry (LPB-in-tree
follow-up). The **slab** form (Plan 0320 implemented) is dense
over the **live-ordinal** count `S = bucket.degree()`; the
property stream has `degree × w` bytes and the descriptor
publishes `slab_slots = degree`. Tombstoned edge slots (counted
in `bucket.stored_slots` but NOT in `bucket.degree()`) are NOT
part of the slab property stream. ADR 0008 profile SSOT owns the
tombstone-aware read backfill semantics; the slab stream is the
dense live prefix and the tree stream is the tombstone-inclusive
1:1 mapping.

- `w = 0`: no property root, no property blocks — zero cost (pay-as-you-go; an
  orientation with no tree buckets has an empty LTB store).
- `w > payload`: tree promotion is rejected (typed error); inline properties
  are compact traversal-critical values by contract, so this is a declared
  bound with a widening trigger, not a supported case.
- Width transitions stay **fail-closed exactly as today**
  (`InlinePropertyBytesWidthMismatch`); tree mode does not regress them. A
  `materialize_inline_property_stream(bucket, w, fill)` primitive
  (reserve `S × w` bytes in the byte-slab allocator; commit fill + one
  descriptor republish) is recorded as **implemented (Plan 0320,
  2026-09-01, slab form)**: the byte-slab allocator is shared with
  inline property bytes, so the reservation is incremental and the
  descriptor publish is one atomic write. The stepped fill uses a
  new `MaintenanceWorkItem::MaterializeInlinePropertyStreamV2` opaque cursor
  (ADR 0021) for reservations above `INLINE_MATERIALIZE_BYTE_BUDGET`
  (= 4 KiB, one VirtualMemory page). The tree form (property root
  + `ceil(S/K)` LPB blocks) reuses the fill/cursor machinery; it is
  recorded as a separate deferred transition (LPB-in-tree, see
  Later Slices). Value backfill semantics stay above LARA (ADR 0008
  profile SSOT; ADR 0058/0059 flavor). `w1 → w2` re-encoding and
  `w → 0` teardown are separate deferred transitions.

  - **Plan 0320 width-addition (0→w) wiring**: the insert path
    helper `ensure_bucket_inline_property_schema_for_insert` now
    delegates the `(bucket_width == 0 && degree > 0)` case to
    `materialize_inline_property_stream` (inline fast path for
    small reservations; deferred stepped fill for large). Other
    transitions (w1→w2, w→0, tree-mode property insert) stay
    fail-closed. The deferred path returns the typed error so the
    caller drains the maintenance queue and retries; no deferred-
    insert completion mechanism is introduced (Plan 0320 §0.8).
- The log blob path (`value_blobs`) is uninvolved: tree mode never logs values
  (§6). The `value_blobs` asymmetric reopen rule is unchanged.

### 6. Overflow log: root ids only

At the CSR layer a tree bucket is an ordinary bucket whose "edges" are block
ids arriving at 1/B the rate. The standard lifecycle therefore applies
unchanged: root growth appends in-window when the leaf has room, else appends
the **block id** to the shared per-leaf log (edge root ids → edge log, 4-byte
cells; property root ids → inline-property log, u32 inline in the 8-byte cell,
blob path unused). Maintenance rebalance folds the log and rewrites the span
to the exact derived length (gap-0 restored; property-root shift happens
here). Edge and property **values** never enter a log in tree mode.

Invariant (per stream, fail-closed on access):

```text
span_len + log_len == derived root len
logical root = span entries ++ log entries (order-preserving, no holes)
```

Unfolded root ids are log-capped (≤ 170 ⇒ ≤ ~174K newest edges behind a
bounded chain walk until the next fold — the same read shape slab buckets have
today). Deepen and promotion always fold.

### 7. Mode machine and execution model

Four bucket-backing states, three transitions: `tiny → slab → tree`, plus
the orthogonal vertex-level `bypass → bucket` entry (ADR 0096). Tiny is the
pre-slab inline state for degree ≤ 3 buckets (descriptor-resident targets,
zero slab slots; see ADR 0096 §1–§5 for wire, birth/promotion, and dispatch
rules). No direct `tiny → tree` edge (the dispatcher chains the two existing
transitions in one insert, mirroring bypass→bucket→tree). No `slab → tiny`
demotion in the first slice (a small slab bucket is merely unoptimized, never
incorrect). Tree promotion/demotion below is unchanged.

**Promotion trigger is `stored_slots`, not live degree** — the capacity bound
requires it (live-degree triggering admits `stored_slots > T_promote` via
tombstone churn: insert 4,095 → delete 2,000 → insert 2,000 appended = live
4,095, stored 6,095). At the crossing: compact in place when tombstone slack
is high (e.g. `live ≤ stored/2`), else promote. Constants carry hysteresis and
are benchmark-tuned.

`promote_bucket_to_tree_mode` follows the `promote_bypass_to_bucket_mode`
failure-atomic template: reserve mints all blocks (`ceil(S/B)` edge +
`ceil(S/K)` property) — all `Memory::grow` completes here; commit transcribes
the slab prefix **and unfolded log entries in logical order** into blocks,
writes the root region, publishes the descriptor, and only then releases the
old edge span (via the vertex-span rewrite) and the old property span (to the
byte-slab `FreeSpanStore`). Promotion is also the moment the leaf releases up
to `T_promote` slots of pressure.

**In-place root (2026-10-30, Plan 0405 `p239`):** promotion is a shrink — the root
always fits the prefix it replaces (`ceil(S/B) + property_root <= S`) — so the
commit writes the root region at the bucket's own base instead of allocating a new
span. That keeps the label-order address monotonicity the successor windows rely
on, allocates nothing, and turns the old "release the source prefix" into "return
the unused prefix tail" (a pin-sheltered prefix is not released at all; the leaf's
derived occupancy reclaims it). Allocation stays a growth operation (bucket append,
deepen, demote). Atomicity: the root write overwrites the head of the source
prefix, so every fallible step — block minting, descriptor publication preparation,
source/canonical/counts validation — completes first, and the root write is the
last mutation before the infallible publish; a failure before it leaves the source
intact, and a failure after it is impossible by construction.

**Implementation note (2026-09-20, GAP-2026-09-20-001):** the commit folds the
bucket's slab edge overflow log into the prefix first
(`ensure_label_bucket_folded_to_slab`) and then transcribes that prefix, rather
than interleaving log entries during transcription. Both satisfy the clause
above; the fold route reuses the existing fold/span-rewrite machinery and keeps
transcription a plain prefix copy. The fold is a hard precondition — tree mode
has no log (§6) — so a slab bucket whose log is still unfolded is never
promoted: before this fix promotion published `overflow_log_head = -1` while
transcribing only `stored_slots`, orphaning the log-resident rows (`degree >
stored_slots`, unreachable from every scan, inserts already returned `Ok`).

Execution model — bounded transitions are synchronous, O(S) work is stepped:

| Transition                          | Bound              | Execution                       |
| ----------------------------------- | ------------------ | ------------------------------- |
| Promotion                           | O(T_promote) ≈ few M ins | synchronous, in the crossing call |
| Deepen / flatten                    | O(R_max) = 4 KiB   | synchronous                     |
| Block mint/release                  | O(1)               | synchronous                     |
| Demotion | Current unsafe implementation: O(S), synchronous in deletion | [Unresolved reclamation proposal](#planned-position-preserving-demotion-and-deletion); no native demotion queue exists |
| `materialize_inline_property_stream` | O(S × w)           | stepped/resumable (ADR 0021)    |
| Full ordered compaction of a tree bucket | O(S)          | stepped maintenance             |

The promotion invariant ("no slab bucket exceeds `T_promote`") is what makes
synchronous promotion safe: the migrated bucket is always threshold-sized,
never supernode-sized. Stepped operations must keep every intermediate
published state fully valid (thin-sliced reserve/commit).

**Batch (ADR 0045):** first slice classifies tree-mode buckets (and
threshold-crossing runs) as `Unsupported` → existing scalar fallback; the
scalar path performs promotion. Widening batch admission to tree buckets
(reserve n blocks → write → publish) is a natural follow-up slice.

**Plan 0321 status (2026-09-02):**
- **Step 1 (fail-closed guard)**: the tree-mode classification
  guard is now implemented in `preflight_run`
  (`OneOrientationBatchError::TreeModeBucketRunUnsupported`).
  Runs whose target bucket is in tree mode are rejected BEFORE
  any geometry math (the planner would otherwise read the root
  region as slab slots and corrupt the bucket on commit). The
  corruption hole (Plan 0319 Step 3 rewrite hazard class) is
  closed.
- **Step 2 (tree-run admission, minimal first slice)**:
  tail-fit runs are admitted through the batch boundary
  (`RunDestination::Tree { tail_block_id, tail_offset_bytes,
  run_edge_count }`). The run writes its edges into the existing
  tail block via `write_payload_partial` and the descriptor is
  published with `stored_slots += n` and `degree += n` in one
  canonical write. Runs that exceed tail room are rejected with
  `OneOrientationBatchError::TreeRunExceedsTailBlock` (caller
  falls back to scalar inserts). Multi-block + root-growth
  widening is a recorded follow-up.
- **Step 3 (threshold-crossing)**: batch writes past
  `T_PROMOTE` are kept working. The next scalar insert promotes
  the over-sized slab bucket to tree mode. Regression test
  `batch_run_past_t_promote_then_scalar_promotes_correctly`
  covers the descriptor correctness. NO new typed error is
  added for threshold-crossing.
- **Batch-remove + LPB-in-tree batch + promote-then-batch**:
  recorded as later slices, not implemented.

### 8. Reopen and validation

Safety is anchored at **allocation time**, not reopen:

- **Pop-time guard (mandatory, O(1) per mint):** popping the free list checks
  `id < tail_next` and `kind == Free`, then rewrites `kind` before returning.
  Live-block aliasing and free-list cycles are structurally unable to hand out
  a block twice — the second visit fails closed.
- **Reopen (fail-closed, bounded):** magic/version/`payload_bytes`/`R_max`
  against build constants; reserved zero-guard; counter consistency
  (`free_count ≤ tail_next ≤ block_capacity`, `tail_next < 2³²`); backing-size
  guard (`HEADER + capacity × 4112`); free-list walk up to
  `min(free_count, declared envelope)` with bounds/kind/cycle checks — an
  early-detection layer only, declared à la GAP-2026-07-11-005 and cheaper per
  entry than the FreeSpanStore merge validation. No O(blocks), O(buckets), or
  O(edges) work at reopen.
- **Access-time (fail-closed, O(1)–O(root)):** per-stream derivation equation
  (§6) and block-header kind/owner/ordinal/level agreement on dereference.
- **Test-only:** full reachability audit (every non-free block reachable from
  exactly one root; free list disjoint).

The LTB stores join the graph's composite all-or-nothing region set with the
`value_blobs`-style asymmetric rule: Fresh ⇒ LTB empty; Reopen ⇒ empty (no
tree buckets) or populated.

### 9. Constants registry

| Constant             | Value  | Kind                                                        |
| -------------------- | ------ | ----------------------------------------------------------- |
| `BLOCK_PAYLOAD_BYTES` | 4,096  | wire (header truth; power of two)                            |
| `R_max`              | 1,024  | wire (header truth — it defines derived depth of stored data) |
| `T_promote`          | 1,024  | policy (benchmark-gated, hysteresis with compact-or-promote; re-tuned 2026-09-20) |
| `MAX_DEPTH`          | 3      | policy (fail-closed structural boundary; widening = future ADR) |
| Log cap              | 170    | existing wire                                                |
| `TINY_MAX_DEGREE`    | 3      | wire (descriptor truth — raising K needs new descriptor bytes, i.e. a layout ADR; see ADR 0096 §3) |
| LTB VMM bucket policy | 16 pages | policy (ADR 0043 experimental; footprint-gated; superseded-by-code from 64 pages — see §1 note) |

`R_max` is deliberately wire, not policy: a build with a different `R_max`
would re-derive different depths for the same `stored_slots`.

**§Decision amend (Plan 0318, 2026 interim cap)** — The production
insert path checks the **physical** root region length against
`R_max = 1024` BEFORE any state change. When the next insert would
push the physical root past `R_max`, the call returns
`LabeledOperationError::TreeRootCapacityReached { stored_slots,
root_len, cap }` (new variant, see `crates/ic-stable-lara/src/labeled/graph/error.rs`).
**No state is mutated on this path** — the guard fires before any mint,
span allocation, or descriptor publish.

**Effective tree-mode cap until the interior-level insert cascade ships**:
`2^20 = 1,048,576` slots per label bucket (= 4 MiB of edge data per
label, = `R_max * B` where `B = 1024`). Once the right-spine cascade
ships (follow-up todo `tree-mode-interior-level-insert-growth`), the
guard is replaced by the cascade and the effective cap grows to
`2^30 = 1,073,741,824` slots per bucket (= 4 GiB).

The structural formulas (`derive_depth`, `root_len`) are unchanged:
they continue to return the *minimum* root length for a given
`stored_slots`. A bucket at `stored_slots = 1,048,576` still reports
depth 1 / root_len 1024 even if its physical layout has been
restructured to depth 2 (one interior) via `tree_mode_deepen`. The
depth-generic resolver and the production insert guard consult the
**physical** depth (`LabelBucket::tree_mode_physical_depth()`, stored
in the `inline_property_bytes_log_len` byte repurposed for tree-mode
buckets) rather than the structural formula, so a manually-deepened
bucket is handled correctly.

## Consequences

- Leaf relocation, weighted rebalance, and placement costs become
  constant-bounded per bucket; supernode mass moves off the PMA rope entirely
  (blocks never relocate).
- No giant contiguous allocations on either slab; `FreeSpanStore` requests
  concentrate in small size classes; LTB allocation is fragmentation-free by
  construction.
- Block-internal reads remain raw contiguous 4-byte rows — the property that
  made the slab beat the B-tree in ADR 0022 — at +1 bounded indirection per
  block (amortized 1/1024 per sequentially scanned edge).
- Logical position contracts (`BucketEntryPosition`, `EdgeHandle`, ADR 0048
  counterpart scan, ADR 0052 ordering/tombstone semantics) are unchanged;
  Graph/Router surfaces are untouched.
- The derived-geometry discipline (span, depth, root lengths all functions of
  `(width, stored_slots)`) makes inconsistent states unrepresentable and keeps
  validation O(1) per bucket.
- ADR 0022 Stage 2 is closed: 2a subsumed, 2b stays rejected.

## Trade-offs

- A third bucket backing mode exists; every bucket-access path carries one
  mode dispatch (single point by contract).
- Point reads inside the unfolded log window pay a bounded (≤ 170) chain walk
  until maintenance folds; random ordinal access pays one block dereference
  per level (≤ derived depth); tail blocks carry ≤ 4 KiB internal
  fragmentation per stream per tree bucket, plus `payload mod w` residue per
  property block.
- The reserve/commit surface grows (promotion, deepen, flatten, block
  mint/release ordering).
- Inline-property width addition remains unsupported (unchanged), with the
  migration primitive only recorded as planned.
- `stored_slots`-triggered promotion may promote churn-heavy, low-live-degree
  buckets "early"; tree mode handles them correctly and the alternative
  (unbounded tombstoned slab spans in the leaf) is worse for the capacity
  goal.

## Measurement gates (pre-implementation)

Per ADR 0022's discipline — benchmarks decide necessity and thresholds.
Implementation does not begin until these pass:

1. **Problem sizing:** degree sweep (4K / 64K / 1M) measuring leaf
   relocate/rebalance/batch-expansion instruction cost and free-span
   fragmentation with a hub bucket in the current layout.
2. **Operation parity matrix:** evidence-only block-tree prototype vs slab vs
   the recorded B-tree numbers, in ins/edge (or ins/op), over: sequential full
   scan, prefix scan, **random ordinal access** (the block-transition and
   per-level-dereference cost — the structure's real exposure, distinct from
   sequential scans that dereference once per 1,024 edges), counterpart
   resolution, insert, delete, and compaction. Headline gate: full-scan within
   ~20% of slab ins/edge (placeholder to be confirmed), against B-tree's 6.3×;
   the other rows are recorded evidence with per-row verdicts.
3. **Promotion cost:** one promotion at `T_promote`, measured for **both** the
   edge-only case and the widest realistic inline-property profile (e.g.
   `w = 32`: 16 KiB edge + 128 KiB property transcription — the cost is
   O(`T_promote` × (4 + w)) bytes, not O(`T_promote`) alone); gate ≤
   single-digit M instructions for the widest admitted profile.
4. **LTB reopen envelope:** declared free-count envelope with measured
   ins/block, following the FreeSpanStore precedent.

If the sweep shows only modest wins, the honest outcome is to reopen this
decision (amend or reject) — consistent with the 2a/2b verdicts.

### Gate results — Plan 0313 (2026-08-30, `StableBTreeMap<BlockId, Vec<u8>>` scaffold)

- **Gate 1 (problem sizing):** *amend.* Slab `bench_labeled_stage2_hub_insert_grow_<deg>`
  at 4K = 28.74M ins, at 65K = 105.97M ins; 1M cargo-test sweep deferred
  (wasm export-name budget saturated). The headline is "current layout
  scales worse than linearly with degree", but the Tree-CSR prototype
  itself cannot be validated against slab at 1M until the budget is
  relieved (see Plan 0314).
- **Gate 2 (parity matrix):** *defer.* Plan 0313's prototype used
  `StableBTreeMap<BlockId, Vec<u8>, VectorMemory>` as the block store; every
  block read and write paid a `Vec<u8>` clone. The Plan 0313 numbers are an
  upper bound on what a real LTB store will measure, not a verdict.
- **Gate 3 (promotion cost):** *defer / explicit-fail-for-headline.*
  `tcsr_promote_edge_only` = 315.74M ins, `tcsr_promote_inline_property_w32`
  = 987.88M ins. Both far past the single-digit M target; the
  `StableBTreeMap` scaffold dominates.
- **Gate 4 (LTB reopen envelope):** *defer.* `ltb_reopen_*` was scaffolded
  on a `StableBTreeMap<BlockId, BlockRecord>` keyed free list — an indirect
  representation, not the raw-block walk ADR 0088 §1 specifies.

### Gate results — Plan 0315 (2026-08-30, raw-block `LtbRawBlockStore<M>`)

- **Gate 1 (problem sizing):** *amend (carried forward from Plan 0313).*
  Slab baseline unchanged. The Tree-CSR prototype's 1M cargo-test sweep
  (`tree_csr_high_degree_test.rs`) is `#[ignore]`d in Plan 0315: the
  raw-block `LtbRawBlockStore::mint` grows by `BLOCK_STRIDE` = 4112 bytes
  per block; 1,048,576 mints exhaust the `VectorMemory` test backend's
  process heap. On real ICP stable memory the same growth fits (canister's
  32 GiB stable page budget covers > 1M blocks). A PocketIC-backed
  high-degree sweep is a later slice; the 4K / 65K canbench arms are the
  verdict anchor for Plan 0315.
- **Gate 2 (parity matrix):** *amend (per row).* The raw-block backend
  (`crates/ic-stable-lara/src/labeled/ltb_raw_block_store.rs`, pub(crate),
  generic `M: Memory`, 12 unit tests including header round-trip,
  magic / version / `payload_bytes` / `R_max` / reserved-zero
  re-open validation, counter consistency, free-list walk with bounds /
  kind / cycle checks, pop-time guard panic, and a 4096-byte payload
  round-trip) replaces the `StableBTreeMap` scaffold. Per-row canbench
  ins/op at 4K / 65K:

  | Row                                          | Plan 0313 scaffold ins/op | Plan 0315 raw-block ins/op | Verdict |
  | -------------------------------------------- | ---------------------------: | ---------------------------: | :-----: |
  | `tcsr_4096_full_scan_descending`             |                    ~59.57 M  |                     169.47 K |  amend  |
  | `tcsr_4096_random_ordinal_access` (×51 calls)|                          n/a |                      10.68 M |  amend  |
  | `tcsr_4096_insert_grow`                      |                    ~214.20 M |                      69.90 M |  amend  |
  | `tcsr_4096_delete_half_by_slot_then_scan`    |                          n/a |                      24.02 B |  reject (prototype-only) |
  | `tcsr_65536_full_scan_descending`            |                          n/a |                       2.71 M |  amend  |
  | `tcsr_65536_random_ordinal_access` (×51)     |                          n/a |                      14.18 M |  amend  |
  | `tcsr_65536_insert_grow`                     |                          n/a |                       1.12 B |  amend  |
  | `tcsr_65536_delete_half_by_slot_then_scan`  |                          n/a |                       6.14 T |  reject (prototype-only) |

  The full-scan row collapses from ~14,540 ins/edge (Plan 0313 baseline) to
  ~41 ins/edge at both 4K and 65K — the headline gate target ("within
  ~20% of slab baseline") is met trivially because the slab baseline is
  the shared-leaf copy path, not the per-edge scan path. The
  `delete_half_by_slot_then_scan` row reports the prototype's naive
  O(N²) shift cost (one `remove_at` per deleted slot, each shifting up
  to N−1 entries left); production tree-mode tombstones live in the slab
  span per ADR 0088 §7, so this is a measurement-only artifact and the
  gate is closed at the layer the ADR scopes. `random_ordinal_access`
  runs 51 calls per iteration and reports per-iteration total — the
  per-call ins count is ~210K, dominated by the deterministic splitmix
  state update (the xorshift shift steps).
- **Gate 3 (promotion cost):** *pass after Plan 0316 (block-batched
  writes).* The I/O amplification that dominated Plan 0313's
  `StableBTreeMap` scaffold (per-slot read-modify-write of a 4 KiB
  block) and the per-write payload constant in Plan 0315's
  `LtbRawBlockStore` (one `Memory::write(4096)` per slot, even when
  only 4 bytes changed) were both eliminated by rewriting the
  `promote_from_slice` and `promote_from_slices_with_property` commit
  phases to fill one `[u8; BLOCK_PAYLOAD_BYTES]` per block in a stack
  buffer and call `write_payload` once per block. The I/O amplification
  collapsed from `O(S)` per slot (4 KiB read + 4 KiB write per slot) to
  `O(B)` per block (one 4 KiB write per block). Plan 0316 also added
  `LtbRawBlockStore::write_payload_partial(id, offset, src)` (bounds
  check; `Memory::write(base, src)` for the sub-range only) for
  callers that want to write less than a full block without going
  through a stack buffer. The bench gate:

  | Bench                                    | Plan 0313 scaffold ins/op | Plan 0315 raw-block ins/op | Plan 0316 block-batched ins/op | Target | Verdict |
  | ---------------------------------------- | ------------------------: | --------------------------: | -----------------------------: | -----: | :-----: |
  | `tcsr_promote_edge_only`                 |                 315.74 M  |                    52.44 M  |                      152.82 K  | ≤ 9 M  |  pass   |
  | `tcsr_promote_inline_property_w32`       |                 987.88 M  |                   105.39 M  |                        1.16 M  | ≤ 30 M |  pass   |

  Edge-only: 343× better than Plan 0315, 2,066× better than Plan 0313
  scaffold, **59× under the single-digit M target**. w=32: 91× better
  than Plan 0315, 851× better than Plan 0313 scaffold, **26× under
  the 30M target**. The 4 KiB block write at ~300 ins/block (PocketIC
  `Memory::write` syscall cost) × 4 edge blocks = ~1.2K ins for the
  edge commit phase alone; the w=32 property commit adds 32 property
  blocks × ~300 ins = ~9.6K ins, which dominates the w=32 bench at
  ~1.16M total (the remaining cost is reserve-phase zero-writes and
  bench setup). The amortization claim
  ("O(`T_promote` × (4 + w)) bytes, not O(`T_promote`)") holds.
- **Gate 4 (LTB reopen envelope):** *pass.* `ltb_reopen_4096` =
  1.30M ins for the free-list walk on a 4096-block envelope =
  ~317 ins/block. The Plan 0314 precedent `bench_r_fs_ro_4096`
  (`bench_lara_free_span_store_reopen_4096`, FreeSpanStore reopen with
  `init` running the full `validate` + cross-check) = 11.60M ins =
  ~2,830 ins/block. LTB reopen is ~8.9× cheaper per block because the
  walk is over a dense intrusive list in cached header bytes, not a
  `StablePagedOrderedMap` whose by-start index must validate, sort, and
  cross-check against the records header. The pop-time guard
  (`ltb_pop_remint_repop_4096` = 9,747 ins) round-trips through a
  kind-rewrite pop, release, and re-pop in under 10K ins.

  Verdict per cell:

  | Cell                                              | ins/edge | Verdict |
  | ------------------------------------------------- | -------: | :-----: |
  | `tcsr_4096_full_scan_descending`                  |    ~41   |  pass   |
  | `tcsr_4096_random_ordinal_access` (per call)      |   ~210K  |  amend  |
  | `tcsr_4096_insert_grow`                           |  ~17,070 |  amend  |
  | `tcsr_4096_delete_half_by_slot_then_scan`         |   ~5.9M  |  reject (prototype-only) |
  | `tcsr_65536_full_scan_descending`                 |    ~41   |  pass   |
  | `tcsr_65536_random_ordinal_access` (per call)     |   ~278K  |  amend  |
  | `tcsr_65536_insert_grow`                          |  ~17,090 |  amend  |
  | `tcsr_65536_delete_half_by_slot_then_scan`        |  ~93.7M  |  reject (prototype-only) |

  Per-row verdict narrative:

  - **full_scan** — *pass* (both 4K and 65K). The raw-block backend
    eliminates the `Vec<u8>` clone that dominated Plan 0313's scaffold;
    ins/edge collapses from ~14,540 to ~41 (350× improvement). The
    absolute number is below the slab baseline ins/edge (slab
    full-scan at 1024-degree = ~14,200 ins/edge from
    `bench_labeled_stage2_hub_scan_descending_1024` in
    `crates/ic-stable-lara/canbench_results.yml`), which is the
    point of the design.
  - **random_ordinal_access** — *amend*. Ins/op is ~210K (4K) /
    ~278K (65K) for the 51-call iteration closure. The deterministic
    splitmix state update (xorshift 13 / 7 / 17) costs ~150–200 ins
    per call, which dominates the per-call measurement. The actual
    block dereference is one `Memory::read` of 4096 bytes plus the
    4-byte read at the right offset — the design's cost exposure is
    correct, but the bench closure includes the bench's own pseudo-
    random generator. Acceptable as evidence; an absolute-count
    verdict defers to a follow-up bench that measures the dereference
    alone.
  - **insert_grow** — *amend*. ~17,070 ins/edge at 4K, ~17,090 ins/edge
    at 65K — bounded by the per-block `Memory::write(4096)` overhead
    (each insert writes 4 bytes but pays for the full payload-write
    constant because the prototype writes the whole block on every
    commit). The design's per-insert amortization is preserved (no
    quadratic growth); the constant is the implementation issue
    flagged for Plan 0316.
  - **delete_half_by_slot_then_scan** — *reject (prototype-only)*. The
    prototype's `remove_at` shifts left in O(N²) (one `remove_at`
    per deleted slot, each shifting up to N−1 entries). Production
    tree-mode tombstones live in the slab span per ADR 0088 §7, so
    the bench is measuring a layer the ADR explicitly does not scope.
    This row is recorded as a known prototype limitation, not as a
    verdict against the design.

  Net Gate 2 verdict: **amend**. The full_scan and insert_grow rows
  that map directly onto the design's promise both pass or amend with
  directional improvement; the delete_half row is a prototype-scope
  limitation.

- **Gate 3 verdict:** *pass.* Edge-only: 152.82K ins, 59× under the
  single-digit M target. w=32: 1.16M ins, 26× under the 30M target.
  The I/O amplification collapse from `O(S)` per slot to `O(B)` per
  block was the design's amortization claim; the bench confirms it
  holds. Plan 0316 closes the blocker that Plan 0315 flagged. The
  full-canister implementation slice (Plan 0317) is now unblocked
  from the Gate 3 side.

- **Gate 4 verdict:** *pass.* LTB reopen walk at 4096-block envelope is
  ~317 ins/block, 8.9× cheaper than the FreeSpanStore reopen
  precedent (`bench_r_fs_ro_4096` = ~2,830 ins/block).

### Implementation readiness (post Plan 0316)

Gate 3 *passes*. The full-canister implementation slice is now
unblocked from the cost side. Plan 0317 must:

1. Wire `LtbRawBlockStore<VirtualMemory>` into `LabeledLaraGraph::new`
   (one store per orientation: forward and reverse), mirroring the
   existing `LaraStore` boundary.
2. Re-run the Gate 2 canbench parity benches on the production
   `VirtualMemory` backend (the bench numbers in this ADR are on
   `VectorMemory` test backend; production `Memory::write` may differ
   in constant cost by ±20%).
3. Run a PocketIC-backed 1M-degree sweep (the cargo-test backend hits
   `VectorMemory` heap limit at 1M; real canister stable memory does
   not) to close Gate 1's deferred row. Until this anchor is
   measured, Gate 1 stays `amend (carried forward)`.

Per Plan 0316 §Notes: `read_payload_partial` and the chunk-buffer
iterator API (`for_each_chunk<F>`) are not in scope for Gate 3 but
are recorded as Plan 0322 for the partial-read side. Gate 2's
amend rows (splitmix closure in `random_ordinal_access`, per-write
payload constant in non-promotion inserts) are recorded as design-
scope evidence, not as blockers, because the gates that *do* matter
for the production wire-up (full_scan, promotion) pass.

## Migration

None. Pre-production fresh-state policy: the layout ships as the canonical
current layout; existing stable state is not converted. The encoding is
factually additive (reserved flag bit zero = slab mode; empty LTB reopens
under the asymmetric rule), but compatibility is not claimed or tested —
fresh state is required.

Implementation order: Labeled LARA first (this ADR); normal LARA later as a
second instance of the same contract (planned); batch admission widening and
demotion as follow-up slices.

## Design Documentation Impact

| Document                                             | Update                                                                                      | Status            |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------------- | ----------------- |
| [adr/README.md](README.md)                           | Index ADR 0088                                                                               | this patch        |
| [storage/lara.md](../storage/lara.md)                | Link from Related documents (this patch); tree-mode contract section, stores table row, consensus-checklist items (on implementation) | this patch / impl |
| [adr/0022](0022-degree-driven-hub-edge-storage.md)   | Close Stage 2: 2a subsumed by ADR 0088 bounded footprint; 2b remains rejected                | this patch        |
| [adr/0007](0007-stable-memory-layout.md)             | MemoryId inventory: LTB store × 2 orientations                                               | on implementation |
| [adr/0052](0052-per-label-adjacency-order-and-tombstone-reuse.md) | Cross-reference: block-local swap-compaction, tree-mode tail-trim prohibition   | on implementation |
| `crates/ic-stable-lara/README.md`                    | Tree mode summary                                                                            | completed (Plan 0318) |

## Required Axes Impact (adr-review)

- **Encapsulation:** the LTB store is crate-private LARA state behind the
  existing graph-owned boundary; Graph/Router see no new surface. The mode
  dispatch is confined to one constructor.
- **Separation of concerns:** rope/PMA/placement stay mode-blind and operate
  on spans; block lifecycle is owned by the LTB store; logical-position
  contracts stay where they are.
- **Invariants:** all new invariants (gap-0, dense root, packing, derivation
  equations, log-holds-root-ids-only, tail-trim prohibition, capacity bounds)
  are enforced by LARA, the owner of the corresponding state, and are
  fail-closed at mint/access/publish sites.
- **Consistency:** no second source of truth is introduced — depth, span
  width, and root lengths are derived from the descriptor; the root array is
  the only block reachability map; sidecars keep their single logical-position
  update path.
- **Fitness for purpose:** the design answers ADR 0022's recorded gap
  (contiguous reads + bounded mutation/placement) for a concrete storage
  problem, without generalizing beyond it (no generic collection type, no
  cross-label sharing, no public API).

## Status

**Plan 0318 implemented (Steps 1-11, 2026-09-01).**

- Steps 1-3 (LabelBucket tree-mode flag bit, LtbRawBlockStore field, cap
  helpers): commits `e70f43534`, `a8ad189d3` on `plan-0318` lane.
- Step 4 (`promote_bypass_to_tree_mode` failure-atomic transition):
  `44c82d3b2` + amend `c2a92f84d` / `c03aac7d4` (stored_slots trigger,
  `BucketNotFound` error, below-cap tests, LTB lazy-create reopen).
- Step 5 (tree-mode read dispatch, single dispatch point):
  `753c100cf` (4 files, 6 tests).
- Step 6a (tree-mode insert dispatch + promotion/cap wiring):
  `1ff13800e` (5 tests).
- Step 6b (tree-mode tombstone remove + Phase 3c leaf-physical
  deferred release): `5918c8d79` (4 tests).
- Step 7 (`tree_mode_deepen` / `tree_mode_flatten` / depth-generic
  resolver): `7918f9be3` (4 tests).
- Step 7 amend (interim fail-closed `TreeRootCapacityReached` guard
  in the production insert path; effective tree-mode cap = 2^20 slots
  per bucket until the interior-level insert cascade ships):
  `d5657eb5f` (2 tests).
- **Plan 0325 (right-spine cascade)**: replaced the 2^20 interim
  guard with the right-spine insert cascade. The fail-closed
  boundary moved to the documented `TREE_STRUCTURAL_CAP = 2^30`
  (depth 1 → 2 at stored = 2^20; fail-closed at stored = 2^30 + 1
  with depth 2 + root_len = R_MAX). Production depth never exceeds
  2; `MAX_DEPTH = 3` remains the primitive safety bound
  (`TreeDepthLimitReached`). canbench surface: guard bench
  `tcsr_1048576_root_capacity_reached` REPLACED by
  `tcsr_1048576_deepen_beyond_r_max` (1M + 1 insert SUCCEEDS via
  deepen) + new `tcsr_1048576_deepen_then_interior_grow` (1M →
  1M + 1024: 1 deepen + 1023 interior-row appends + 1 new-interior
  mint). Synthetic-layout unit tests prove the 2^30 fail-closed
  boundary (canbench cannot seed 2^30: 17.9T ins > 10T limit).
  Wasm 15,661 chars (was 15,536; +125 for the 2 new bench names).
- Gate 2 re-run (Step 9): all production-relevant benches within ±1%
  of Plan 0315/0316 baselines (full_scan, insert_grow, promote_*);
  see `plans/0318-tree-csr-implementation.md` §Step 9 for the
  results table.

**Follow-ups (status: pending)**:

- ~~`tree-mode-interior-level-insert-growth`~~ — **resolved in Plan
  0325** (this slice). Right-spine cascade at depth ≥ 2 ships; the
  effective cap is 2^30 slots per bucket. Depth 3 remains
  structurally wired (`MAX_DEPTH = 3`) but the production
  fail-closed boundary at 2^30 makes it unreachable; lifting the
  cap to 2^40 (depth 3 in production) is a future ADR amend.
- ~~`tree-mode-tombstone-reuse`~~ — **resolved in Plan 0340** (this
  slice). Tree bucket tombstones are now reused on insert for
  **Unordered** tree buckets via header-count-guided, tail-first
  bounded-window reuse (`tree_mode_reuse_tombstone_slot`, Plan 0340):
  the ADR 0094 per-block header `tombstone_count` satisfies the
  follow-up's "needs per-bucket free-ordinal tracking" prerequisite
  (no `LabelBucket` field used). The FIXED 4-block window keeps
  `stored` bounded under tail-churn (measured by the `tree_churn_*`
  attribution bench), answering "does demotion alone suffice?" with
  **no** — high-degree buckets (`degree > T_DEMOTE = 2,048`) never
  demote, so without reuse their LTB footprint grows monotonically.
  **Insertion tree buckets never reuse** (ADR 0052 §6); batch tree
  runs stay append-only (recorded later slice).
- `tree-mode-promote-from-bypass` — bypass-mode → tree-mode direct
  transition (current path is bypass → slab → tree). Records as
  a follow-up so the current two-step path is not blocked on
  one-step direct promotion design.
- PocketIC-backed 1M-degree sweep (Plan 0318 Gate 1 deferred row;
  the 1M-sweep is `#[ignore]`'d in `tree_csr_high_degree_test`
  because it hits production `Memory::grow` limits in test
  environments).

**Plan 0319 (Steps 1-3, 2026-09-01) — historical demotion + rewrite-path
checkpoint.** The slab→tree transition gained `tree_mode_demote_to_slab`,
reported at that checkpoint as a failure-atomic reserve/commit/publish primitive.
That general atomicity/deletion claim is not established: the
[2026-09-22 prerequisite audit](#property-bearing-deletion-prerequisite-2026-09-22)
finds property collection and movement-publication defects. The primitive rebuilds a
tree bucket as a fresh CSR slab of only live edges and releases all LTB leaves + interiors
and the old root region. Demotion fires inline at the single tree-mode
remove dispatch point via the `T_DEMOTE = T_PROMOTE / 2 = 2048`
degree-hysteresis trigger (`remove.rs:328`). The rewrite path
(`vertex_edge_span_retire_intervals`) adopts the new
`bucket_span_region_len` helper so tree buckets contribute their
physical root-region length (not `stored_slots`) to per-vertex
span intervals; this closes the latent Plan 0318 corruption
hazard where a rewrite reaching a tree bucket misread the root
region as degree edge slots. Commits: `8598874d8` (Step 1,
primitive + 5 tests), `08f6f9d6c` (Step 2, T_DEMOTE + trigger),
`4a71edaee` (Step 3, region-length helper + audit). Full rewrite
verbatim moves (copy loops skip tombstone scans for tree buckets)
are a recorded follow-up.

GAP-2026-07-25-002 parallel track: the per-bucket occupancy map
idea (compressed tombstone distribution / persistent live bitmap
for OFFSET scans) remains future work; demotion is the primary
reclaim path for this slice.

**Plan 0320 (Steps 1-3, 2026-09-01) — `materialize_inline_property_stream`
slab form.** The §5 primitive is now implemented: reserve `S × w`
bytes through the byte-slab allocator, fill with `fill_byte` via a
stepped resumable cursor (ADR 0021, `MaintenanceWorkItem::
MaterializeInlinePropertyStreamV2`), publish the descriptor
(`width + offset + slab_slots + log reset`) and update the
vertex-level accounting in one atomic window. Width addition
(0→w on non-empty buckets) is wired through the insert path
helper `ensure_bucket_inline_property_schema_for_insert_
with_materialize`; the inline fast path (≤ 4 KiB) runs
synchronously, the deferred stepped path (Step 2) returns the
typed error so the caller drains the maintenance queue and
retries. Other transitions (w1→w2, w→0) stay fail-closed. Commits:
`772960947` (Step 1, primitive + 6 tests), `c5ae29a97` (Step 2,
wiring + work item + drain test). **Plan 0326 (LPB-in-tree, this
slice) ships the tree-mode property insert path** (see §Status
below); the remaining w1→w2 / w→0 transitions on tree buckets
stay fail-closed (typed `InlinePropertyBytesWidthMismatch`).

**Plan 0321 (review rework, 2026-09-02) — F-1/F-2/F-3 fix.**
The F-1 must-fix closed the `tail_offset == 0` boundary hole in
the batch tree branch (rejects `TreeRunExceedsTailBlock` when
the tail block is exactly full and the next block does not
exist yet). F-2 added `is_tree_mode` to `BucketFingerprint` so
slab↔tree flips between reserve and commit are detected. F-3
replaced the tree path's `panic!`/`expect` with `debug_assert!`
+ `eprintln!` (the function returns the infallible
`OneOrientationBatchResult`, so the typed error cannot propagate;
release-build fail-silent with debug surface).

**Plan 0324 (2026-09-02) — PocketIC 1M sweep — 6/6 benches PASS; read-dispatch bug GAP-2026-09-02-001 fixed in this slice.**
The `#[ignore]`d prototype host tests in
`tree_csr_high_degree_test.rs` are removed (user decision
2026-09-01). All 6 production-surface benches implemented and
PASS deterministically (3-for-3 each):

| Bench | Result | ins | ins/edge |
|---|---|---|---|
| `tcsr_131072_insert_grow_below_cap` | **PASS** | 5,306,066 | n/a (write batch) |
| `tcsr_131072_full_scan_descending` | **PASS** | 6,235,765 | 47.6 |
| `tcsr_1048576_insert_grow_below_cap` | **PASS** | 4,749,914 | n/a (write batch) |
| `tcsr_1048576_full_scan_descending` | **PASS** | 49,859,317 | 47.5 |
| `tcsr_1048576_random_ordinal_access` | **PASS** | 50,650,893 | 48.3 |
| `tcsr_1048576_root_capacity_reached` | **PASS** | 3,326 | n/a (1 guard insert) |

The read benches (full_scan, random_ordinal) initially TRAPped
deterministically (3-for-3) at
`ic-stable-structures-0.7.2/src/vec_mem.rs:36:13` with `read: out
of bounds`. Root cause: 3 dispatch sites in `traverse.rs` (visit_edges
line 786, visit_edges_window line 1577, visit_edges_with_inline_property
line 1823) used a "dense, tombstone-free" fast path that
bulk-reads `degree × E::BYTES` bytes from `bucket.edge_start()`.
For tree mode, `edge_start` is the LEG root region (block_id array,
`root_len × 4` bytes), NOT the LTB payload blocks — so the bulk
read went OOB.

**Fix applied in this slice:** tree-mode `is_tree_mode()` branches
added at all 3 sites, routing to `tree_mode_out_edges_collect` (LTB
walker) or `visit_slab_bucket_span_edges` (LTB-aware, mode-dispatched slow path). The
`FnMut(u32, E) -> FnMut(BucketEntryPosition, E) -> ControlFlow<B>`
adapter is implemented as a `Vec<E>` collect + re-iterate (the LTB
primitive's `FnMut(u32, E)` signature doesn't match the public API's
`FnMut(BucketEntryPosition, E) -> ControlFlow<B>`). Other read
paths (`visit_edges_for_label_impl` line 1970,
`visit_edges_for_label_with_inline_property`, inline-property
bulk-read paths) are tree-mode safe by construction (require
`inline_property_byte_width > 0`, which is 0 in tree mode) or
already have tree branches.

**Regression test added:** `tree_visit_edges_via_public_api_works_on_dense_tree_bucket`
in `tree_read.rs:447`. Exercises `graph.visit_edges` on a 4K-
promoted tree bucket via the public API, both ascending and
descending. Verified to FAIL without the fix (first visit in
desc reads 0 instead of 4195, garbage from LEG region) and PASS
with the fix (3-for-3).

**Why Plan 0318 Step 5 didn't catch the bug:** the Step 5 read-
dispatch tests used `tree_mode_out_edges_collect` directly (the
LTB primitive), not the public `graph.visit_edges` API. The public
API has its own dispatch at line 786 that unconditionally tried the
dense fast path before falling through. The Step 5 tests also
used bucket degrees below the LEG root size (4096 edges = 4 LTB
blocks = LEG root 16 bytes), so even a misread from `edge_start`
returned within the (small) memory window without trapping. The
1M bench exposed the bug because 1M = 1024 LTB blocks = LEG root
4096 bytes, while the dense read attempted 4MB.

Wasm: 15,536 / 20,000 chars (4,464 headroom). Green-bar:
**620 / 620 / 583 (lib / no-default / canbench feature) all green
(+1 regression test)**; clippy -D warnings clean; fmt clean.
**Full canbench suite: 158/158 unchanged, 0 regressed, 0 failed**.
4K/65K prototype baseline: 0% regression (6/6 unchanged). Validator
final phase PASS. GAP-2026-09-02-001 moved to Resolved.

**Initial BLOCKER-001 claim retracted (REWORK-1).** The first audit
attributed the trap to non-deterministic seeding failure above 77K
edges. Orchestrator REJECT noted the contradiction (the 131K
insert_grow bench worked 5-for-5, requiring seeding past the
alleged 77K boundary). REWORK-2 re-root-caused the trap as a
read-dispatch bug and fixed it in this slice.

**REWORK-3: position contract fix (2026-09-02).** The REWORK-2
fix used `tree_mode_out_edges_collect` + `enumerate()` to forward
edges to the visit closure. This yielded **live-ordinal**
positions (0..degree), which matched the tombstone-inclusive slot
only when `degree == stored_slots` (dense tree buckets). For
tombstoned tree buckets (`degree < stored`), the positions were
compressed, violating the ADR 0088 §2 contract (BucketEntryPosition
is tombstone-inclusive bucket-local). REWORK-3 replaces the
adapter with a direct call to `visit_tree_mode_label_bucket_edges`,
forwarding the tombstone-inclusive `u32` slot directly. As a bonus,
this removes the Vec materialization, **reducing read bench
instructions by ~50%**: 1M full_scan 49,859,317 → 24,729,578, 1M
random_ordinal 50,650,893 → 26,273,315, 131K full_scan 6,235,765 →
3,093,866. Dispatch order in `visit_edges` and `visit_edges_window`
reordered: dense condition first (more selective, with
`!is_tree_mode()` guard), tree branch after. Regression test
`tombstoned_tree_bucket_visit_preserves_logical_positions` added
in `tree_read.rs:511` (promotes 4K bucket, tombstones slot 100,
verifies visit yields position 100 with tombstone in both orders).

**Plan 0326 (LPB-in-tree, 2026-09-02; REWORK-3 layout fix) — §2 tree-form property
stream ships; promote carve-out removed.** The Plan 0318 §Step 4
amend's interim `InlinePropertyBytesWidthMismatch` for `w != 0`
buckets is removed; the promote path now accepts `0 < w <=
payload_bytes` (declared bound per §2) and transcribes the slab
property stream into a depth-1 LPB tree (LPB blocks share the LTB
store with kind `BlockKind::InlineProperty`). **Layout (REWORK-3,
single contiguous span):** the vertex span holds `[edge root |`
`inline-property root]` (gap 0) — the property root is the `u32`
block_id array at `edge_start + edge_root_len`, DERIVED (never a
stored offset). Invariants after promote: `w` preserved;
`inline_property_bytes_offset = 0`, `inline_property_bytes_slab_slots
= 0`; depth byte holds the edge-tree physical depth (the property
tree's depth is derived from `(w, stored_slots)` — property leaves
hold `K = floor(payload_bytes / w)` values each; property interiors
are B-radix, so the property tree's own coverage differs from the
edge tree's). Tombstone semantics: per §2, tombstoned slots retain
their property value bytes; the `is_tombstone_edge` filter applies
to the edge tree only. Both insert sub-paths (edge-leaf boundary and
tail-room) mint property leaves at K boundaries and grow the
property root via the combined-span realloc; `write_property_value_at_slot`
never resolves past the root (K-boundary mints precede it).
**Interim per-w slot cap** (property-tree deepen not yet wired —
follow-up per §7): a `w > 0` tree bucket's effective slot cap is
`min(2^30, K × R_MAX)` (w=32 → 131,072; w=4 → 2^20 exactly),
enforced by the typed `PropertyTreeRootCapacityReached` guard;
`tree_mode_deepen` relocates the property root when the EDGE tree
deepens (w ≤ 4 reachability), and the combined-span realloc keeps
`[edge root | property root]` contiguous on every growth. The `bucket_span_region_len`
helper gains a companion `property_root_region_len` so the
compaction rewrite path can sum edge + property roots for
`w > 0` tree buckets. `w > payload_bytes` remains a typed
`InlinePropertyBytesWidthMismatch` reject (the ADR-declared bound).
Out of scope for this slice: 0→w on existing tree buckets,
w1→w2 re-encoding, w→0 teardown, depth-2 property tree, batch
`commit_with_location_mode` tree admission for w > 0
(recorded as batch scalar-fallback carve for a future slice).
Regression tests: `lpb_in_tree_read_round_trip_at_w_4_stored_4096`
(promote with w = 4 at T_PROMOTE = 4096 succeeds;
`inline_property_byte_width` preserved), and
`lpb_in_tree_property_leaf_fanout_matches_documented_table`
(K = floor(4096/w) at w ∈ {1, 8, 32, 128, 1024}). 627 / 627
library tests pass (was 625; +2 LPB-in-tree tests, 0 removed).

**Plan 0327 (property-tree deepen, 2026-09-03) — per-w slot cap lifted to
`2^30`; `PropertyTreeRootCapacityReached` is now the d' = 3 backstop.**
The depth-1 property tree fail-closed at `property root len = R_MAX`
(w=32 → 131,072 slots; w=4 → 2^20). The production growth paths
(`tree_mode_insert_edge` tail-room, `tree_mode_tail_append_depth1`,
`tree_mode_tail_append_depth_ge2`, `tree_mode_deepen`'s property-root
anticipation, and promote) now cascade: when the next property leaf
would overflow the root at the current property depth `d'`, the root's
entries are packed into `ceil(root_len / B)` new `BlockKind::
InlinePropertyInterior` blocks and the combined span is reallocated as
`[edge root | new property root]` (edge root bytes copied verbatim;
property root offset stays derived at `edge_start + edge_root_len`).
Property-root region length at depth `d'` is `ceil(L_p / B^(d'-1))`
with `L_p = ceil(S / K)`; effective per-`w` cap is now `min(2^30,
K × R_MAX × B^2)`, i.e. the edge tree's `TREE_STRUCTURAL_CAP = 2^30`
for every `w ≥ 1` (d' = 3 suffices for all widths).
**Depth representation amendment to §4:** the property-tree physical
depth is **stored**, packed into the tree-mode depth byte (bits 0-1 =
edge physical depth − 1, bits 2-3 = property physical depth − 1; all
access via `LabelBucket::tree_mode_physical_depth` /
`tree_mode_property_depth`). The §4 "pure function of stored_slots"
derivation is kept as the structural formula, but the physical marker
is authoritative for resolver hop depth — tombstone compaction can
shrink `stored_slots`, so a derived property depth would under-walk a
deepened root (the same reason the edge tree keeps its physical depth
marker). A derived-depth design was audited and rejected: it requires
an eager property-tree flatten on every downward boundary crossing
with no hysteresis. Latent bug fixed in the same slice:
`tree_mode_tail_append_depth_ge2` previously ignored `w > 0` entirely
(both the missing K-boundary leaf mint and the edge-root-only realloc
that would orphan the property root); all its branches now perform the
combined-span realloc. 546 / 490 library tests pass (was 541; +5
property-deepen tests, `lpb_in_tree_rework_f2_cap_guard` retargeted to
the 2^30 backstop).

**Plan 0327 polish (2026-09-03, same day) — `visit_edges_window`
contract unified to tombstone-inclusive positions.** The pub(crate)
window API previously applied `offset`/`limit` in live-ordinal space on
the slab paths (bypass, dense, sparse) but in tombstone-inclusive
position space on the tree path, so the same window returned different
edges for slab vs tree buckets holding tombstones — the asymmetry
recorded by Plan 0324 REWORK-3. All paths now cut the window in
request-order positions over the bucket's tombstone-inclusive extent
(ascending position = slot id; descending position = `extent − 1 −
slot` with `extent = stored_slots + overflow-log len`), yielding only
live edges; tombstoned positions consume window space on every mode.
Consumer audit: `TraversalWindow` / `visit_edges_window` consumers are
crate-internal only (grep over `crates/graph` and `crates/gql`: zero
hits), so the semantic change breaks no external caller. The
`t_off_*` window benches and `bench_t_v_window` re-encoded to the
position contract (`canbench --persist` full suite, 142 entries,
0 regression; `bench_t_v_window` improved 10,574 → 9,669 ins, −8.6%,
from replacing `try_advance_by` buffering with direct position cuts).
D-1 (the +2.10% `bench_t_v_window` regression attributed to Plan 0324
REWORK-3 dispatch) is closed: re-measurement at the 0327 HEAD showed
the 0326 dense-first dispatch reorder had already absorbed it.

## Tree visitor short-circuit (implemented)

Verified: 2026-09-12 23:34:39 UTC +0000.

The topology and inline-property tree visitors propagate `ControlFlow::Break` to the storage
walk. After the callback breaks, neither walker decodes another slot, resolves another tree
leaf, nor reads another property value. The current edge leaf has already been read into a
fixed 4 KiB buffer; this is not a byte-at-a-time I/O guarantee. An error in an unvisited leaf is
not reported by a stopped traversal; a complete traversal still reports errors it encounters.

Ownership remains in `tree_read.rs`; `traverse.rs` forwards the stop result directly instead
of remembering it while the underlying walk continues. Full collectors and demotion explicitly
continue. Ascending/descending order, tombstone-inclusive positions, and the property bytes of
the visited slots are unchanged. No storage format, MemoryId, per-edge metadata, normal write
algorithm, or activation/reinstall requirement changes.

The native regressions `tree_visit_break_stops_topology_reads` and
`tree_visit_break_stops_property_reads` poison a later root reference, assert exact values and
an owned Break result in both directions, and verify that uninterrupted traversal detects that
same invalid reference. Topology stops cover the first slot and both sides of a leaf boundary,
including a tombstone at the stopping position. The property case stops before an unreadable
property leaf while the next edge remains in the already-read edge leaf.

Focused, unpersisted canbench (`tree_visit_stop`, production insertion/promotion, dense depth-1
buckets; setup excluded) measured:

| Stored slots | Stop after first slot | Full callback walk |
| --- | ---: | ---: |
| 4,097 | 11.83K instructions | 268.10K instructions |
| 65,537 | 11.83K instructions | 4.11M instructions |

All four measured closures reported zero heap/stable-page increase; that does not mean their
fixtures or read buffers occupy zero memory. The existing production
`tcsr_131072_full_scan_descending` benchmark measured 3.09M instructions, −0.03% against its
persisted baseline (within noise). These are traversal measurements, not bulk SET throughput,
inline-property performance, or a fresh same-revision before/after comparison.

This correction is one prerequisite for the [planned bulk edge SET direction](../investigations/2026-09-12-bounded-edge-membership-witnesses.md#selected-direction-and-storage-accounting),
not a complete work bound. Overflow-chain reconstruction/prefetch, tombstone work before the
first callback, property and outbox byte budgets, local mutation preflight, and retained
positive/zero/rejected outcomes still require their own contracts. Logical-slot paging or
callback-count limits alone must not be advertised as a bounded all-match mutation.

## Tree property slot bound (implemented)

Verified: 2026-09-13 01:26:43 UTC +0000.

`resolve_property_leaf_block_id` returns `EdgeSlotOutOfRange { slot, stored_slots }` when the slot
is outside the bucket's physical extent, before root or property payload reads. It no longer
returns block ID zero as a sentinel: zero may be a minted block belonging to another stream.
`tree_property_slot_bound_rejects_block_zero_alias` reproduced the old successful zero result and
now asserts the typed rejection. Valid-slot resolution and the storage layout are unchanged.

The [bounded complete inline reader](0050-lara-traverse-read-api.md#bounded-complete-inline-property-collection-implemented)
uses this existing resolver with topology-selected physical slots and an admitted body-byte budget.
Its property-tree fixture invokes promotion explicitly and seeds an encoded tombstone; it does not
claim that property-bearing tree mutation dispatch is implemented. No new persistent bytes or
activation/reinstall requirement is introduced.

## Threshold re-tune (implemented 2026-09-20)

`T_promote` moved 4,096 → **1,024** slots, with `T_demote = T_promote / 2 = 512`
(`crates/ic-stable-lara/src/labeled/graph.rs`). Both are policy constants: the
wire layout, the LTB format, and every persisted descriptor field are unchanged,
so no reopen or migration is involved — the flip only moves where a slab bucket
becomes a tree bucket.

Evidence (canbench, unpersisted arm comparison, both fixed first for the two
promotion defects below; full table in
[implementation-gaps §GAP-2026-09-17-001](../implementation-gaps.md) and the
[LARA improvement investigation](../investigations/2026-09-17-lara-improvement-investigation.md)):

| Metric (IC instructions) | `T_promote = 4096` | `T_promote = 1024` |
| --- | --- | --- |
| M1 full-path hub growth 0 → 8192 (equal work) | 55.87 M | **41.70 M** |
| M2a full descending scan of a 2048-edge bucket | 74.30 K | **40.09 K** |
| M2b steady-state append into a 2050-edge bucket | 8.36 K | **4.46 K** |
| M2b append landing on a block boundary (2048 → 2049) | **8.36 K** | 63.51 K |
| M2c delete from a 2048-edge bucket | **4.84 K** | 7.33 K |
| G5 skewed workload mix (256 vertices / 4520 edges) | 140.11 M | **132.24 M** |
| M4 promote → demote → re-promote round trip | 117.65 M | 30.77 M (threshold-relative sizing) |
| Drain a 1,024-edge hub, all deletes (`bench_l_s2_det_hub_1024`) | 25.01 M | 25.01 M (parity) |
| Drain a 4,096-edge hub, all deletes (`bench_l_s2_det_hub_4096`) | 102.59 M | 102.59 M (parity) |
| Property-bearing scan, 4,096 rows, w = 32 (`tcsr_4096_property_read_w32`) | **3.22 M** | 4.83 M → 3.66 M after the per-leaf property cursor (GAP-2026-09-20-003 fix) |

**Attribution (measured 2026-09-20 with temporary scopes).** Only one of the
three cost rows above is caused by the re-tune:

- **Property-bearing reads** (w > 0): the only measurable re-tune cost left.
  Same bench, same 4,096 rows, same w = 32: 3.22 M with a slab bucket vs 4.83 M
  with a tree bucket before the per-leaf cursor, 3.66 M after it
  (GAP-2026-09-20-003) — a +13.7 % tree indirection cost (one leaf resolution
  plus one 4 KiB block read per leaf). It does not show up in the w = 0 scan
  result (M2a, which favors tree mode) because there is no second stream to
  walk.
- **Hub drain**: *not* caused by the re-tune, and now at parity between the
  arms. The original +147 % reading was F1's per-emptied-bucket span release
  (`release_bucket_edge_span_on_empty`, commit `3fd14768b`, ~30 K instructions
  per emptied bucket) — threshold-independent, since the emptied buckets are
  slab in both arms. The drain release batch (GAP-2026-09-20-002, commit
  `a021a309c`) merges those ranges per delete and removed the cost entirely, so
  both drain benches now measure **identically at 1,024 and 4,096** (25.01 M and
  102.59 M). What remains above the pre-F1 20.72 M is F1's necessary per-delete
  cover sync (~4.2 K per emptied bucket, vertex-row accounting), not a re-tune
  effect.
- **The once-per-B-rows block-boundary mint** is a genuine re-tune cost, bounded
  to one mint per B appended rows.

So the re-tune is a workload-shape decision: it favors pure-adjacency ingest and
scan (including the Orkut-shaped G5 mix) at parity on deletes, and costs +13.7 %
on property-bearing scans of buckets above `T_promote` rows plus the bounded
once-per-B block-boundary mint. Revisit trigger: a property-scan-heavy target
workload — re-run the equal-work benches above (2,048 keeps 4,096-row property
buckets on the slab side). A promoted bucket's
resident root region also shrinks with the threshold (§4: depth-1 `root_len =
ceil(T_promote / B)`), from 4 LEG slots to 1, which reduces the leaf pressure
each tree bucket contributes. The re-tune was gated on
two promotion defects found while measuring: the false post-promotion density
cascades (GAP-2026-09-17-001's density-accounting half) and promotion dropping
overflow-log-resident rows (GAP-2026-09-20-001).

Test migration: the ten tests that encoded 4,096/2,048 collapse into
`T_promote`-derived sizes (batch tail-fit seeds and widths, promote root-length
reads, the demote-hysteresis removal loop, the inline-property materialization
scenario, which now uses wider rows to stay above the materialize byte budget
while below the cap). The suite is green at both constants (602 / 602), so a
future re-tune does not require touching them again.

## Property-bearing deletion prerequisite (2026-09-22)

**Status: live-deletion implementation paused; mutation assertion and demotion unchanged.**
The [valid-backed investigation](../evidence/adr0097-tree-deletion-prerequisites.md)
reproduced the width-zero assertion through public post-promotion deletion. Its temporary
assertion-only diagnostic confirmed dead-property retention and exact 8/33-byte
Insertion append / Unordered reuse above the demotion floor, but also exposed:

- Already-dead public deletion returned `Some(tombstone)` because its existence owner
  probed LEG rather than LTB. The subsequent point-state repair below fixes that read/no-op
  boundary; the matching-removal mutation path still requires separate repair.
- At live degree 512, demotion collects 1024 property rows against 512 live edges and
  panics after deletion has committed. Its successful path also packs survivors without
  returning their movement events; that later failure is source-audited, preempted by
  the measured property panic. Combined-root reservation, publication/error ordering
  and property-tree retirement need review in the demotion owner, not just a value filter.

GAP-2026-09-22-001/003 remain open in the [implementation ledger](../implementation-gaps.md).
GAP-002 producer/header validation remains separate. The approved slab-compaction repair
preserves tree siblings; it is not evidence of tree mutation or demotion safety. No threshold,
wire layout or fresh-state requirement changed.

### Planned: position-preserving demotion and deletion

**Status: Planned, revised after REQUEST CHANGES; reclamation policy remains unresolved.**
Revision anchor: 2026-09-22 13:17:45 UTC +0000. The 12:00:21 draft is preserved as reviewed history
in the [source audit and revision record](../evidence/adr0097-demotion-design.md). Neither draft
supersedes the shrink contract or authorizes implementation. The recorded 513→512 failure and
current assertion remain unchanged. Stage A's physical-publication safety work is independent of
approval of any demotion policy.

#### Reopened decision and representability

A no-move deletion must remove exactly the selected occurrence, decrease degree/global accounting
once, and preserve survivor positions and values. That safety constraint does **not** establish a
new right to retain tree form permanently. Keep `T_DEMOTE = 512` and `T_PROMOTE = 1024`; for a pending
delete let `D = degree - 1` and `S = original stored_slots`. Eligibility is evaluated before mutation.
**Unchanged numeric thresholds do not mean unchanged shrink behavior.** In the rejected proposal,
`S = 1025, D = 512` would never demote while nonempty even with ample capacity. That is an unapproved
policy change, not merely error handling. The retained-extent rule is withdrawn, not implemented
as a temporary suppression branch.

The old target was specifically **spill-free**: `S` initialized prefix slots, no edge run, and `D`
dense values. Its `S <= 1024` admission is not a limit on every slab representation. Existing slab
prefix ownership and total initialized extent are different: a prefix of `P` and a used run of `R`
represent `P + R` original positions; neither allocator capacity nor live degree substitutes for
that sum. With the existing prefix cap and `MAX_ROWS = 1024`, `S = 1025` can, at the representation
level, use `P = 1, R = 1024` without renumbering or raising a cap. This is an existing run format,
not a fabricated sparse identity map. Source conversion, target acquisition, promotion admission,
value placement and later reclamation for that shape still need proof. For targets constrained to
`P <= 1024` and one run `R <= 1024`, the envelope is `S <= 2048`, not an arbitrary tree high-water.

Consequently `S > 1024` alone is neither capacity failure nor a deletion disposition. The alternatives
below compare a prefix/run target, movement-bearing explicit operations and existing resumable
maintenance before considering any new progress mechanism. Reclamation timing, progress under idle
or failing capacity, and behavior beyond the existing target envelope remain decision gates. A
transient retained-capacity outcome is also a proposal whose retry/liveness contract needs approval;
it is not licensed to become permanent retention. No cap, threshold, schema, compatibility path,
new persistent state, hidden move vector or unsafe no-move wrapper is authorized.

The last live deletion must not discard roots that still own resources. A synchronous implementation
would prepare complete retirement or reject before the deletion; that is a safety condition, **not**
a liveness solution. The historical-extent work bound below remains an unresolved gate. Repeating a
successfully deleted slot returns `None`, not a reclamation retry receipt. Existing queued work may
not be declared finished or dropped merely because a conversion was not performed.

#### One aligned selection

`tree_write.rs::tree_mode_demote_to_slab` remains the conversion owner. Use the existing native
property-aware visitor (width zero has no property access), original positions and canonical
liveness semantics. In one selection pass, account for the pending deletion and build:

- For a position-preserving target, the edge at original slot `i`, with holes and the pending
  deletion encoded as tombstones. Partitioning that image between prefix and run does not change
  `i`. A separately approved packed operation instead needs the complete old→new mapping; selection
  alone does not publish it. Never sort/deduplicate by target: parallel occurrences are distinct.
- The live value sequence: append the full value at `i` if and only if that same edge is selected
  as surviving. Live value ordinal `j` belongs to the `j`th surviving original position, not to
  property slot `j` in the old tree. Validate exact survivor count and `D * w` bytes before writes.
- The removed occurrence with its original slot, storage label and supported inline bytes for
  existing delete observers. Retaining tree form leaves its dead property's bytes untouched;
  converting to slab legitimately drops dead values because slab values are dense over live rows.

Supply the pending deletion to this same selection without first tombstoning the source. A ready
conversion publishes the new image directly; it does not commit an intermediate tree deletion.
The existing explicit demotion/test entry has no pending deletion: use current degree as `D`,
leave global count unchanged, and share this preparation/publication path rather than retaining
a second demoter. Later native wiring prepares all accounting before writes: conversion adds `D`
to slab-live; a pending deletion decreases global count once; explicit conversion does not. A
retained-tree deletion, if its disposition is approved, does not change slab-live.

Above-threshold deletion prepares the requested native row/value and marker/header/count changes;
it is not required to scan every unvisited root. Encountered corruption still propagates. A retained
path must not be advertised as full-tree validation. `EdgeRemoval<E>` is a committed result, not
ownership of unpublished targets.

Use fallible buffer reservation and checked lengths, not a post-write debug assertion. Raw native
visitors remain tombstone-inclusive. The rule is owned here, not copied into remove wrappers,
Graph observers or a property-only filter.

#### Physical ownership and the outer publication unit

Source `S` is **not** the source LEG allocation. `bucket_resident_region` / combined-root geometry
owns `[edge roots | property roots]` using each physical depth marker; width zero has no property
roots. Future conversion must inventory leaves and **every interior level** before retiring any
root, rejecting duplicate/free/unminted IDs and invalid geometry. Root IDs remain addressing truth;
this does not certify GAP002 headers or every plausible minted cross-bucket alias. Target constructors
must describe actual initialized prefix/run extents and independently dense live values. A spill-free
target uses `stored_slots = S`; a prefix/run target uses prefix `P` plus used run `R = S - P`. Neither
constructor nor degree can stand in for old ownership. Pin-owned subspans are not individually free;
standalone unpinned regions and the old leaf cover must each be retired exactly once.

`compact.rs` remains the single physical publication owner. **The unit is one outer rewrite or
whole-leaf slide/relocation call, including all peers it can change**, not one invocation of its
12-argument vertex commit helper. This is not an entire insertion, maintenance pass, paired Graph
delete or IC transaction. An insertion may have changed state before entering this unit.

| Existing flow | Current earliest canonical publication | Required prepared unit |
| --- | --- | --- |
| Weighted slide / relocation expanded in place | First source-overlapping copy, otherwise first changed row/count write; expansion's free-prefix reservation is not adjacency publication | Every affected normal/bypass/tree peer, endangered source bytes, final rows, old/new cover/counts and cancellation of any acquired expansion |
| Disjoint leaf relocation | `set_segment_physical_start` changes the pin **before** the peer loop; later peers can still fail | New reservation remains unpublished; every peer and the final pin/count transition are prepared before pin or row publication |
| Rewrite with no relocation | First overlapping prefix/root write, or the first row/origin publication for a disjoint target | Requested vertex, complete destination layout, affected ownership/accounting and retirement |
| Rewrite whose base resolution relocates peers | The nested relocation's pin write, not the later requested-vertex commit | Relocation **and** the requested final rewrite form one unit; compute the latter against the planned placement, never publish relocation and restart |

Remove the nested publish/restart in `rewrite_vertex_edge_span_with_policy` and lift preparation
outside both peer loops in `rebalance_labeled_leaf_weighted_slide_in_block`. A vector of independently
committed vertices is not atomic preparation. Failure in the last peer must leave every earlier
peer canonical at its old coordinates. Before the first canonical write, the owner must have:

1. Captured canonical source rows, mode-specific physical regions, old pin, segment/count index and
   all affected peers, including bypass projections. Plan the complete final layout with existing
   placement/leaf geometry; preserve weights, headroom, thresholds and fallback semantics.
2. Checked all slot/byte ranges, descriptor destinations/encodings, source/destination intersections,
   and prospective counters for the whole unit. Physical-only work changes neither global nor slab
   live counts; pinned leaf `total` follows the **cover**, not the sum of bucket replacements.
3. Planned every destination without publishing free-list or element-capacity changes, under the
   allocation authority described below. Prepare exact old-cover/unpinned releases against that
   prospective post-acquisition state. Stable backing growth, serializer/copy scratch, index directory
   space, heap bookkeeping and release validation are obligations, not just `reserve_for_releases(n)`.
   Discard on rejection; do not acquire through today's bare-address facade and compensate afterward.
4. Prepared final vertex/bucket bytes via `LabelBucketStore` and pin/count transitions via existing
   edge/count owners. Keep old ownership separate from final images. No descriptor re-encoding,
   checked count update, source-resource discovery or new reservation may reject after publication.

Afterward, publication consumes the prepared unit once: copy safe payloads, publish all rows and the
pin/count transition, then apply already prepared retirement. Intermediate rows need not be visible
as one hardware write: there is no callback, observer, await, re-entry or recoverable return in that
interval. Internal reads of proven immutable raw source ranges and prepared allocator bookkeeping
are distinct from fresh fallible planning. An unexpected violation aborts; native unwind is not recovery.

**Streaming and scratch:** disjoint means the union of final destination writes is disjoint from
**every** still-needed source in the unit, not merely each peer's own source. After all peers' metadata
and capacities are prepared, stream raw bytes through one reusable bounded buffer. No edge/value
re-decoding or late per-peer allocation is required. Overlap retains the existing buffered strategy:
materialize all endangered raw regions before the first write; buffering the whole affected payload
is permitted there, not the default for a disjoint move. If rewrite uses a virtual relocated layout,
copy directly from original canonical ranges to final ranges rather than exposing an intermediate pin.

Let `V` be participating vertices, `K` their total descriptors, `P` the sum of copied physical bytes,
`Q = max_v(P_v)` the largest participant's physical payload, `R` release intents and `F` allocator
free records. Reserve one reusable `Q`-byte buffer for the disjoint arm, retaining per-vertex streaming
rather than adding a new small-chunk optimization. Disjoint payload scratch is `Q`, plus
`O(V + K + R)` prepared metadata and `34*K` encoded bucket bytes; overlapping payload scratch is
at most `P` plus that metadata. `K <= 65536*V`; `V` comes from the configured segment,
not an assumed 16. `P` counts initialized prefixes/combined roots, not unused capacity, retained-run
payload or native LTB leaves. Allocator scratch is a separate term: its lazy heap can depend on `F`.
All arithmetic and reservations are checked. Do not impose a new graph-size cap to hide a scratch
problem. Record actual capacities, copied/read/written bytes and growth for both arms before/after;
no unconditional whole-leaf buffer or extra full-payload validation scan is accepted without measurement.

The minimal shape is **one private, non-Clone, nonpersisted prepared layout** at this owner, containing
all participant final rows, validated copy ranges, old/new cover and count transition, planned
reservations plus their owner-bound acquisition/retirement image, and the selected raw scratch strategy. A fallible
prepare call returns it; one consuming publish call has no recoverable result. Child descriptor and
release data remain opaque to `compact`; wire/index rules stay in their current stores. Replace the
12-argument helper and its three call sites together; do not add flags, a demotion-only publisher,
parallel safe/unsafe publication paths, or a generic transaction/undo framework.

**Resource-closure gate:** existing reservations pre-grow backing; they do not prepare semantic
index batches, split scratch or heap work. Plan0339 is completed and independently approved for its
[bounded admission repair only](../evidence/adr0097-demotion-design.md#reduced-admission-implementation).
It is not the implementation plan for full A0 or A1. The span acquisition/retirement decision below
remains **planned and uncompiled**. Review `b9bd4e87` authorized only the generic map foundation's
bounded implementation/validation; that candidate now exists but failed every direct IC phase and
the retained LARA control. It is not accepted and does not authorize any later owner implementation.

#### Planned nonpublishing acquisition and retirement

**Selected for review: one prospective acquisition/retirement plan at the existing owners.**
`span.rs` owns destination selection and element-capacity intent; `FreeSpanStore` owns the free
records, coalescing, bins, index edits and heap; the paged map owns its page/directory rules.
`compact.rs` supplies placement and actual retirement intervals, then owns whole-unit publication.
No second allocator, persistent state, new module, undo framework or raw map geometry in LARA.
This protocol covers LEG spans; it does not supply B's value-allocation or native LTB retirement proof.

| Choice | Before final metadata is ready | Cancellation obligation | Decision |
| --- | --- | --- | --- |
| Nonpublishing acquisition/capacity plan | Only private free-record/index/heap images and proposed capacity change; backing may grow | Discard images under the still-held authority; no free-span release or capacity shrink | Selected: one forward preparation/publication path |
| Owner-bound cancellation receipt | Free-list takes and capacity are already published | Reserve complete reverse record/bin/index/directory/heap edits **before** each take; retain free-list versus tail origin, old capacity, all receipts and authority; cancel in reverse order without failure | Not selected: subsequent retirement preparation and receipts need the same prospective index machinery, plus inverse publication |

A receipt containing only `(start,len)` is insufficient. Restoring an exact earlier free-list image
can require reversing a split/rekey, record recycling and heap changes; tail cancellation either
restores published capacity or introduces a free range that did not previously exist. Both require
an explicit contract. Reserving stable pages does not reserve inverse directory edits or heap work.
The selected design avoids this additional state transition rather than hiding it in `Drop`.
A minimal receipt would be one non-Clone batch bound to `&mut SpanWrite`, retaining each span's
free-list/tail origin, old capacity and reserved inverse images, with consuming `cancel(self)` and
commit. Independent receipts borrowing the same writer cannot coexist. Moreover, a reverse map
ready token would already borrow the map needed for forward retirement preparation: supporting both
outcomes requires map-owned alternate images under one authority, not two fabricated ready tokens
or temporarily dropping the guard. That broader two-outcome API is another reason not to select it.

**Backing, capacity and ownership:** physical `Memory::size()` is not persisted/cached
`elem_capacity`, and neither describes initialized adjacency content. During preparation the latter
capacity stays `C0`; destinations removed from a *private* free view are still free in canonical
storage but unavailable to competing allocation because authority is held. A planned tail starts at
prospective capacity `C*`, not physical backing's end. Retirement is applied to that same private
post-acquisition view. On rejection, canonical free/index/record/header/bin bytes, heap contents and
ready flag, both capacity copies, rows, pins and counts remain unchanged. Only unreferenced backing
(including reserve sentinels) and buffer/heap **capacity**, not logical heap contents, may differ.
No initialized source bytes may change. Default buffer deallocation is not allocator rollback.

**Allocation authority:** `EdgeStore::span_write()` first obtains the existing free store's
`by_start.try_borrow_mut()`, then `max_spans.try_borrow_mut()`. Failure releases any first borrow and
returns `BorrowMutError` before writes or growth. No new lock is added. The free-store writer owns
these `RefMut`s; its plan borrows their contents exclusively, and the span plan borrows that writer.
All free-list, restoration, take/pair, reservation and heap-mutating entry points must acquire this
same authority before mutation; private setters move behind the writer/ready image. Internal calls
use held authority rather than recursively borrowing. A pure zero/no-op path need not acquire it.

The barrier must also cover `set_elem_capacity` and every whole-header writer that could republish
capacity. `edges.rs` retains growth/layout arithmetic; `init.rs` retains persisted/cache publication.
A prepared capacity change writes only that field and updates its cached counterpart, never a stale
whole-header snapshot. No unguarded capacity setter, raw slab accessor or alternate allocator writer
may bypass the barrier. Scalar `&self` aliases reject conflicting mutation before their first write
(existing borrow-panic facades may remain pre-write); checked entry returns the borrow error.
A second plan/ready object from one writer is prevented by its `&mut` borrow; an independent writer
conflicts with the held `RefMut`. A consuming ready publisher has no destination-store parameter.
No post-publication epoch check, custom cleanup `Drop`, native unwind recovery or automatic release.
As elsewhere, independently reopened stores/raw-memory writers aliasing the same backing are outside
exclusive region ownership; this is not a new global alias registry.

This authority protects allocation/capacity, not an arbitrary Graph transaction. A1's prepared
physical object remains private to `compact.rs`: prepare and consume occur inside one closed
synchronous outer call, with no caller callback, returned ready object, await or re-entry. Its helpers
must not mutate rows/pins/counts while deriving final metadata. Exposing that object outside this
flow would require additional row/count authority and is **not** licensed by the span guard.

**Minimum surface:** the generic map signatures below exist in the blocked candidate; span/free-store
signatures remain uncompiled sketches. These summaries are not executable examples or full-A0 proof:

```text
// Public, generic, in ic-stable-paged-ordered-map/src/lib.rs only:
Edit = Insert { key: u64, value: u64 } | Remove { key: u64 }
PrepareError = Edit { at: usize, source: Error }
             | Grow { at: usize, source: GrowFailed }
             | AddressOverflow { at: usize }
             | Scratch { at: Option<usize>, source: TryReserveError }
prepare_edits(&mut self, edits: &[Edit]) -> Result<PreparedEdits<'_, M>, PrepareError>
PreparedEdits::previous_values(&self) -> &[Option<u64>]
PreparedEdits::publish(self) -> ()
PreparedEdits::discard(self) -> ()

// crate-visible at EdgeStore/span.rs; all capability fields private:
EdgeStore::span_write(&self) -> Result<SpanWrite<'_, E, M>, BorrowMutError>
SpanWrite::plan(&mut self) -> Result<SpanPlan<'_, E, M>, SpanPrepareError>
SpanPlan::acquire(self, len: u64, avoid: Option<FreeSpan>)
    -> Result<(Self, FreeSpan), SpanPrepareError>
SpanPlan::seal(self, retire: &[FreeSpan]) -> Result<PreparedSpans<'_, E, M>, SpanPrepareError>
SpanPlan::discard(self) -> ()
PreparedSpans::publish(self) -> ()
PreparedSpans::discard(self) -> ()
```

`seal` preserves the input plan's borrow lifetime (the sketch's `'_` is that same lifetime).
Consuming `acquire` means an error discards the entire draft; a caller cannot ignore an error and
seal partially updated scratch. There is no poisoned-state flag or reusable cancellation receipt.
Returning a `FreeSpan` supplies placement coordinates, not independent allocation authority.
Only the bound plan can seal those changes; A1 must retain it until publication/discard.

| Private object / owner | Minimum retained fields |
| --- | --- |
| Map `PreparedEdits` | `&mut StablePagedOrderedMap`, one opaque dirty-range image with inline small storage and fallible overflow storage as specified below; one previous value per intent, inline for zero/one intent |
| Free-span writer | Bound store plus existing index and heap `RefMut`s; no second allocator |
| Free-span draft/ready image | Borrowed writer parts; prospective header/bin/record overrides, ordered map intents and their caller-origin indices while drafting; sealed opaque map preparation, encoded record/bin/header writes and prepared heap update when ready |
| Span writer/plan | Bound `EdgeStore` and free writer; plan borrows those parts, retains `C*`, acquired ranges and the free draft. Acquired ranges exclude accidental retirement of a destination; no origin/inverse-write log is needed |
| `PreparedSpans` | Bound store, slab-owner-backed encoded capacity change, sealed free preparation. Acquired ranges can be discarded after seal validates retirement disjointness |

The structs borrow writer **parts**, not themselves; do not put a guard and a token borrowing that
guard into a self-referential struct. No `Clone`, public constructor, raw edit-image accessor or
redirectable publisher. `FreeSpanStore` supplies prospective operations within its current file;
LARA never decodes or predicts a paged-map split. Map scalar insert/remove and the new public batch
use one private prepare-image/publish-image engine. Scalar `&self` calls prepare and consume
immediately without exposing a token; public preparation retains `&mut self` across the caller's
interval. Delete superseded immediate page/bin edit implementations, not merely wrap them.

##### Scalar dirty-write and scratch contract

**P2 contract; implementation acceptance blocked:** an image means pre-addressed **dirty fields/entry ranges**, not an
obligatory 1048-byte page copy. One shared `apply_edit` rule reads the canonical-plus-pending view and
emits these ranges for scalar and batch calls. Storage size does not select a different editor.
Keep three output-range descriptors, 32 encoded bytes and zero/one prior result inline; retain
fixed-size lookup/header/address locals, not a page payload array, on basic paths. Overflow of those
bounds uses fallibly reserved byte/range buffers; only multi-result batches need a result vector.
No `Vec` conversion merely to pass a scalar edit/result through the batch interface. The candidate's
native image object measures192B. Hash-matched static inspection found a752-byte linear-memory
stack adjustment in the original candidate's scalar function and304B after the authorized correction.
Neither is peak call-chain stack or a Wasm object-size measurement. See the
[correction evidence](../evidence/adr0097-demotion-design.md#caller-owned-image-correction-checkpoint).
The32-byte/three-descriptor limits are encoded-storage bounds, not stack-size claims.

| Operation on valid backing | Reads and retained image | Publication / allocation contract |
| --- | --- | --- |
| Existing-key replacement | Directory/key search, selected header and prior value; retain one encoded value, not the selected page payload | Write only its 8-byte value field, one write; no map/header/key rewrite, map-owned dynamic buffer/reserve or grow |
| Absent remove | Directory/search evidence needed to establish absence; no payload image | Zero writes, map-owned dynamic buffer/reserve or grow; inline `None`. Do not inspect unrelated pages |
| Non-splitting tail insertion into a nonempty page | Lookup and checked lengths; one new 16-byte entry, 2-byte page length, 8-byte map length | Exactly three ranges /26 bytes, inline; no whole-header or old payload rewrite, directory work, dynamic reserve or grow |
| Tail removal leaving the page nonempty | Lookup/prior value and checked lengths; two length fields | Exactly two ranges /10 bytes, inline; do not clear the now-unused entry or rewrite other header fields; no dynamic reserve or grow |
| Interior shift, actual split or directory shift | Read only the affected entry suffix; a split legitimately needs all64 old entries and one inserted entry (1040 encoded bytes). Directory work retains only affected minima/entries and shifted suffix | Reserve overflowing dirty bytes/range descriptors before building them; no second `split_off` payload vector. Reuse one encoded65-entry region for left/right split ranges. Write affected payload/metadata only, not both whole1048-byte pages; coalesce a contiguous directory suffix rather than create a buffer per entry |

Value-only edits and unsuccessful searches do not traverse unchanged neighbor links; validate the
selected page's ACTIVE flag when a page is selected, not every other page's header. This preserves
the later corrupted-page witness without substituting an earlier whole-map guard.
A split may allocate overflow scratch even for one scalar call; results still fit inline. Compute
added byte/range needs per edit and reserve each overflowing collection before filling it; repeated
batch edits may enlarge those collections during preparation, never after ready. Preserve existing
split/directory admission order and all checked geometry. Newly minted pages require backing for
the **full allocated page extent** even when the highest dirty byte is earlier; dirty bytes are not
allocated capacity. These decisions add no persistent format, cache, public mode flag or dependency.

For the same-source basic scalar fixture in Draft0340, candidate read calls/bytes must not exceed
the baseline ceilings; changed-page cloning cannot be excused by functional parity. Map-owned
scratch counters must show zero dynamic use/reserve on the four basic paths, and report dirty-byte/
range high-water. Baseline scratch is a source audit where no observer exists, not a fabricated
runtime zero. The direct IC scalar phases and native traffic oracle are separate gates: neither
proves global allocator activity, compiled stack size or all population sizes. Any needed excess
requires review of the concrete check/cost, not loosening the oracle or calling it unavoidable.

**Historical map checkpoint R (Q earlier).** R `d2980db54ca03b4893b7f6031ec5a6147927e345074250fe3ed8ab733c21298d` = Q plus the bounded local-position contract. R meets its Q-relative controls but still fails **all six original ceilings**; no tolerance, aggregate or budget waiver applies. Full A0/A1, mutation, ownership, reclamation and ADR0097 movement/sidecars/demotion/deletion/matching/shrink/retirement and landing obligations remain open. E `172879d5…` remains the historical performance reference; F/G remain rejected; Q is historical. Evidence lives once in the report's [R checkpoint](../evidence/adr0097-demotion-design.md#frozen-r-execution-checkpoint); the [Q checkpoint](../evidence/adr0097-demotion-design.md#frozen-q-execution-checkpoint) and earlier anchors retain theirs.

`DirEntry` owns directory decoding through its new private `decode` method. Canonical
`read_dir_entry` performs one 16 B backing read; prepared `directory` retains its 16 B prospective
`EditImage` request. `apply_edit` shifts encoded in-page entries without per-entry decoding;
`set_entry` remains the pair-encoding owner. Locator/selected tuple and error guards are unchanged.
`PageHeader` owns page-header mapping; `EditImage` owns dirty ranges and `cover` owns
coalescing, packed-byte relocation and staged-byte preservation. `EditPlan` borrows that image;
`PreparedEdits` retains exclusive consuming publication. Geometry, count, admission, split
pre-covers and E's equality guard/local first-key reuse are unchanged. There is no new public API,
layout, cache, editor, allocator or publication path.

**Local-position contract:** `apply_edit` forwards the full-page absent-key insertion position (`0..=64`) computed in the same prospective view with no intervening write; `split_page` remains the real split owner, keeping `DirectoryFull`-before-directory-scan and active/full/header/count/admit/publish checks, deleting only its repeated search; the cfg adapter transparently forwards the fixture-supplied position `64`.

**Current retained map checkpoint: reserved scratch writes (Plan0362, 4/6 original ceilings).**
Private `Scratch::push_reserved` remains the single initialized-value append owner for dirty-range
creation and per-intent prior results. It does not use bulk resize to create a slot that is
immediately overwritten. Inline's checked element write precedes its length increment; Heap checks
spare reserved capacity before Vec::push. An over-capacity append changes neither length, contents,
capacity nor representation, and a valid append does not construct a discarded Default value.
Bulk `resize_reserved` owns exact mutable live views in one representation match. Inline obtains
its checked array prefix before assigning length; Heap guards reserved capacity before its existing
Vec::resize/default initialization. Rejection preserves length, contents, capacity and representation
without Default initialization or implicit growth. Inline retains hidden initialized slots across
shrink/regrow; Heap defaults newly exposed slots. The capacity accessor is cfg(test)-only.
Fallible reservation and its error ordering are unchanged. No public API, representation, layout,
version marker, codec, scalar-specific editor or publication rule changes.

Source `47796acb` retains the entire resize candidate: replacement and tail improve, missing is equal,
and split plus both LARA controls improve. Replacement and tail still exceed their original ceilings;
4/6 is not full acceptance. Formatting, map lib/tests and LARA lib strict lint,30 map units,
5 doctests and27 FreeSpan tests pass. Both ordering/growth mutants fail at their intended assertions;
the exact candidate is restored before final validation. Existing LARA test-target lint debt remains
open. LARA includes unchanged pending admission work, not clean-commit integration evidence.
The compact layout below is unchanged.
[Measurements, source bindings and limits](../evidence/adr0097-performance-architecture-research.md#16-reserved-resize-retained).

**Rejected allocator-operation trial: order-preserving rekey (Plan0359).** A generic rekey intent
and one FreeSpan replacement owner reduced LARA total/named831214/830253→632242/631281, but
all four map controls regressed: missing88872→89704, replace102792→103624,
split29531→29544, tail381066→383562. The whole candidate, including API/tests and shared-owner
refactoring, was restored; no part of that candidate remains. Acceptance was3/6 at that checkpoint.
Scoped runtime tests pass; strict LARA test-target clippy remains blocked by unchanged test debt.
[Trial and restoration](../evidence/adr0097-performance-architecture-research.md#13-order-preserving-rekey-rejected).

##### Paged-map compact layout

**Implemented and retained: compact page header (Plan0360, 4/6 original ceilings).** The previous
format made selected-page validation read a32B header and then its8B first key, although active and
free links are mutually exclusive. The map now owns one24B tagged header and1048B page stride;
`EditPlan::page` reads header plus first key in one32B prefix. Full-page extent/count/backing checks
still precede that read, and active tag/length/link checks precede directory-minimum agreement.
Canonical reads, prepared/scalar edits, split, neighbor updates and recycling use the same codec.

| Page-relative bytes | Active tag1 | Free tag0 |
| --- | --- | --- |
| 0 | Tag1 | Tag0 |
| 1, 4..8 | Padding: encode zero, ignore on decode | Same |
| 2..4 | Little-endian u16 length,1..64 | Zero length |
| 8..16 | Previous active page, or0 | Next free page, or0 |
| 16..24 | Next active page, or0 | Required zero |
| 24..1048 | Up to64 sorted16B key/value entries | Inactive payload, ignored |

The private raw `PageHeader` keeps the shared link in `prev`; it does not claim validated state or
retain a second free-link field. `PageHeader::free_next()` owns free tag/zero-length/unused-word
validation for both allocation and the full free-list walk. Bounds, self-link and cycle checks
remain at their existing traversal/admission owners. Unknown tags are rejected by active/free
admission. The64B outer header,8192-entry directory and page-id width are unchanged. The existing
version marker1 and public `HeaderV1` type are unchanged too: development layout revisions do not
introduce a new format epoch. Only this current layout is supported. **Use fresh owning memory
bundles when the layout changes; do not rebuild only the by-start region beside retained FreeSpan
records.** No old-layout detection, compatibility decoder or migration is provided. No MemoryId changes.

Keeping the old two reads leaves the repeated work; widening the read to40B violates the existing
32B traffic contract. Changing directories or allocator indexes is a separate decision. This
format fits the existing map owner and changes neither ordering/publication nor FreeSpan selection.
The compatibility break and rederived offset/overflow fixtures are accepted costs of the measured
improvement, not authorization to change allocator policy or add another optimization.

Against0358, missing88872→85352, replacement102792→99272, split29531→29274 and tail381066→374026;
LARA total/named831214/830253→823204/822243. All six improve. Missing clears85672 by320; replacement
and tail still fail77576/298171, so full acceptance remains blocked. Map/LARA Wasm shrink236/362B;
all measured heap/stable page increases remain0. No individual read/check instruction price is
inferred. Source `6bb465d5` passes30 map units,5 doctests,27 unchanged FreeSpan tests and affected
map/LARA-lib strict lint. Its six measurements exactly match the initial compact-header trial after
restoring the unchanged version marker/type. Existing LARA test-target lint debt remains open.
The initial trial's first-key guard bypass failed the corruption oracle; that guard is unchanged.
[Decision, measurements and limits](../evidence/adr0097-performance-architecture-research.md#14-compact-page-header-retained).

**Previous retained map checkpoint: single reservation owner (Plan0358, 3/6 original ceilings).**
Source `4e5456e1` removes the production fit/early-success decision from `reserve_scratch` and
always delegates normal requests to existing `Scratch::reserve`. Scratch alone decides whether
capacity suffices and owns fallible growth and representation changes. Test-only observation still
counts/injects faults for non-fitting requests, but no longer bypasses Scratch on fitting requests.
Result, byte and range reservations share this path; reservation order, error ordinals, checked
geometry, lookup validation, prior values, admission and publication remain unchanged. No helper,
type, parameter, mode, API, layout or second editor is added; Scratch's implementation is unchanged.

Against0357, missing90152→88872 (−1.4198%), replacement105736→102792 (−2.7843%),
split29609→29531 (−0.2634%) and tail386954→381066 (−1.5216%). LARA total/named
832624/831663→831214/830253 (−1410 each, overlapping scopes). Every instruction control improves;
missing remains3200 above its original ceiling, and acceptance remains3/6. Map Wasm shrinks230B;
LARA Wasm grows966B, explicitly accepted for the measured improvement and simpler production path.
All measured heap/stable page increases remain0, not a zero-global-allocation claim.

The existing Scratch fixture now protects exact fitting reservation, overflow failure preservation
and nonempty Inline-to-Heap prefix retention. It passes on old and new production; all28 units,
5 doctests, scoped clippy and both original IC filters pass. A skipped multi-result reservation
fails the existing ordered preparation after setup at the reserved-capacity assertion. Static
scalar/preparation frames remain208/160B, locals2/41; exact component prices remain unisolated.
LARA includes the unchanged pending admission work, not clean-commit integration. Full evidence and
limits: [performance research, section11](../evidence/adr0097-performance-architecture-research.md#11-single-reservation-owner-retained).

**Previous retained map checkpoint: first-cover borrowed window (Plan0357, 3/6 original ceilings).**
Source `d750b055` adds only a positive-empty case inside `EditImage::cover`: obtain the reserved byte
view, fill its one gap, append one descriptor at packed offset0, and return that same borrowed view.
An empty range set has no packed bytes to relocate or merge. `Scratch` retains capacity checks;
callers retain no-op/full/live-prefix gap policies. Zero-count and nonempty normalization paths,
page validation, reservations, error precedence, admission and publication remain unchanged. There
is no new helper, type, mode, public API, persistent state or second editor. All28 tests are unchanged.

Replacement and tail improve9.5284%/4.7723%; both LARA rows improve about1.10%. Missing+256
(4 instructions/call) and split+28 are explicitly accepted, together with map/LARA Wasm growth
3130/6059B, for those benefits. This is a specific measured tradeoff, not a general tolerance or
ceiling waiver. Split and both LARA gates remain passed; missing/replacement/tail remain unmet.
The original source-bound filters, clippy,28 units and5 doctests pass; fresh-zero and Heap48
callback faults are killed by the unchanged cover oracle. Historical F/G remain rejected in their
older source/acceptance context. Full measurements, artifacts and static-frame limits are in
[performance research, section10](../evidence/adr0097-performance-architecture-research.md#10-first-cover-borrowed-window-retained).

**Previous retained map checkpoint: metadata-first bulk insertion (Plan0353, 3/6 original ceilings).**
Source `073473e4` retains0352's bulk movement and changes only private staging order in the existing
non-split insertion arm. After checked count and reservation, `apply_edit` records map length,
directory minimum if needed, then page length before the unchanged payload window and new entry.
The window uses the captured old location/header; it does not reread those staged metadata fields.
Canonical gaps remain bounded by the old live end. No later duplicate metadata writes remain.
`cover` still owns normalization, `set_entry` encoding and `publish_image` the first canonical write
after preparation/admission. Public semantics, errors, dirty destinations and publication order stay.
No new type, helper, signature, branch, allocation, layout or test fixture is introduced.

Against retained0352, tail449930→406346 (−9.6868%), LARA total874351→841909 (−3.7104%) and
named873390→840948 (−3.7145%). Missing89896, replacement116872 and split29581 are unchanged.
This candidate is retained without a regression tradeoff: the LARA increase seen in0351's per-entry
implementation does not occur in this bulk combination. Split and both LARA rows still meet the
original ceilings; missing/replacement/tail remain unmet. LARA total/named are overlapping scopes,
not independent workloads. No absolute ceiling or general tolerance changes.

The28 existing tests are byte-identical. Scalar and20-case non-split characterizations passed before
editing; formatting, clippy,28 units and5 doctests pass afterward. A tail-key-only wrong map length
fails the intended exact header assertion after setup/replace/missing, then is restored exactly.
Source-bound builds and original IC filters pass; all heap/stable page deltas are zero. Map/LARA
Wasm grows6/12B; scalar/preparation frames remain208/160B with2/40 locals. Fresh-tail packed-payload
relocation34→0B is source-derived, not an isolated IC price or proof of the LARA gain's cause.
LARA still includes unchanged pending admission work, not clean-commit integration. Source, raw
streams, frozen compiler-output copies, CSVs and receipt are under
`/Users/yota/.local/share/gleaph/evidence/adr0097/bulk-insert-order-0353/`.

**Rejected experiment: lookup-local directory pairs (Plan0356).** An aligned32B directory buffer
shared by binary search and the below-minimum insertion fallback preserved selected validation and
prospective visibility, with16B reads at odd tails and no cross-intent reuse. The source-bound
original map/LARA fixtures nevertheless regressed on every control, including replacement, so the
candidate and its pair-specific assertions were restored exactly to0353. Scoped clippy,28 units,
5 doctests and both IC filters passed correctness; no performance gain or new contract was adopted.
The measured decision, retained ceilings and immutable artifacts are recorded in the
[performance research, section9](../evidence/adr0097-performance-architecture-research.md#9-adjacent-directory-pair-experiment-rejected).

**Previous retained map checkpoint: interior bulk movement (Plan0352, 3/6 original ceilings).**
`apply_edit` replaced both non-split entry-shift loops with existing borrowed-window operations,
retaining the original metadata order at that checkpoint. Insertion covers the affected suffix plus
the new slot, reading canonical gaps only below the old live end. Removal saves the final prospective16B source and covers
only its dirty destinations, never the unused old tail. Zero-shift paths skip the window. The unused
per-entry decoder was deleted. Source `330260d1` and its tests were retained byte-for-byte from the
measured candidate. Formatting, clippy,28 units and5 doctests passed after restoration. One independently
seeded map covers20 position/source/direction cases with exact priors, dirty-address union, full bytes
and discard. The resource fixture exercises remove/reinsert on unbacked page126 before the same
admission failure. Both archived movement/tail mutants fail at their intended runtime assertions.

Against prior baseline `7ea423ad`, LARA total2014198→874351 (−56.5906%) and named2013237→873390
(−56.6176%) both pass their original ceilings. Tail450058→449930 also improves. Missing89768→89896
(+0.1426%), replacement116744→116872 (+0.1096%) and split29579→29581 (+0.0068%) increase by128/128/2
instructions. These small regressions are explicitly accepted in exchange for the measured LARA
improvement; they are not a failed correctness contract or a reason to discard the optimization.
The original zero-regression hypothesis still fails. Its initial rejection/restoration evidence is
preserved, but the retention decision is superseded by this accepted tradeoff. No absolute ceiling
is relaxed and no general regression tolerance is introduced: split and both LARA rows pass, while
missing, replacement and tail remained open. Retained acceptance reached3/6 at0352.

Source, bound Wasm/CSV, tests and the original rejection remain under
`/Users/yota/.local/share/gleaph/evidence/adr0097/interior-bulk-0352/`; `adoption/receipt.json` records
the superseding decision. The unchanged source/input/artifact hashes permit reuse of those IC
measurements, not a new run. Map/LARA Wasm grew6807/13260B; heap/stable page deltas stay zero. The map
preparation frame grows144→160B while its40 locals and scalar208B frame stay unchanged. The three
regressions are uniformly2 instructions per scalar invocation, but their exact cause is not isolated;
static frame size is neither peak stack nor a per-instruction price. LARA still includes pending
admission work, not clean-commit integration. No new variant, helper, API, layout or editor is added.

The earlier checkpoint measurements below describe their recorded sources, not the current0360 source.

**Rejected: insertion metadata ordering.** Plan0351 moved only the existing non-split insertion
writes after their unchanged reservation: map length, optional directory minimum, page length,
then the unchanged backward entry shift and new entry. The same editor, prospective reader,
normalizer and publication boundary remained; no new type, branch, helper or test fixture.
Candidate `925cdff5` passed clippy, all27 unchanged units and5 doctests. The scalar byte/traffic
oracle also rejected a wrong moved map-length write at the tail header assertion. The first
unconditional mutant instead broke fixture construction; that failed witness placement is preserved
separately, not counted as tail coverage. The corrected mutant targets the existing tail key only.
Against retained `7ea423ad`, tail improved450058→406538 (−9.6699%); missing89768,
replacement116744 and split29579 were unchanged. LARA total2014198→2041282 and
named2013237→2040321 regressed (+27084 each), so the non-regression requirement rejects this
candidate. Both sources meet only the original split ceiling (1/6). Source-derived packed payload
relocation for a fresh-image tail insertion falls34→0B, but that does not price individual copies
or attribute the LARA regression. Map/LARA Wasm shrank21/54B; static scalar/preparation frames
remained208/144B and heap/stable page deltas were zero. LARA retains the same pending admission
work, not clean-commit integration. At the0351 close, the entire lib.rs was restored to `7ea423ad`;
0352 also retained that order;0353 re-evaluates it with bulk movement. Sources, both mutants, raw
streams, artifacts, CSVs and restoration are under
`/Users/yota/.local/share/gleaph/evidence/adr0097/insert-address-order-0351/`.

**Rejected: unmaterialized scratch.** Plan0350 tested a private `Scratch::Empty` state with
logical inline capacity but no initialized payload. Empty/Inline shared one non-heap reservation
path; first positive checked resize materialized inline values. Initialized storage was retained
across shrink-to-zero. No unsafe, public/storage API or publication change was introduced.
Candidatec73c1f64 passed clippy,27 units and5 doctests. A Default-counting extension to the existing
scratch fixture rejected eager construction and covered cold access, zero capacity, materialization,
reservation failure and prefix-preserving transitions. No Memory/map/PocketIC fixture was added.
Against7ea423ad, missing89768→88040 and replacement116744→114696 improved, but
split29579→29959, tail450058→455754, LARA total2014198→2046463 and named2013237→2045502
regressed. The unchanged six-row requirement therefore rejects the candidate. It still met only
split's original ceiling. Map/LARA Wasm grew2420/4248B; heap/stable page deltas remained zero.
The scalar initialization prefix lost five vector payload-zero stores; its static frame remained208B.
This confirms deferred initialization reached codegen, not an isolated per-store price or an
attribution of the other regressions. At the0350 close, the entire lib.rs, including tests, was
restored to7ea423ad (commit4062d5d50), with Inline/Heap only and original acceptance1/6.
Candidate/red sources, bound artifacts, CSVs, restoration and review are under
`/Users/yota/.local/share/gleaph/evidence/adr0097/empty-scratch-0350/`.

**Rejected: preparation input-shape specialization.** Plan0349 tested a private borrowed
`AsRef<[Edit]> + ?Sized` input so scalar `[Edit; 1]` and batch `[Edit]` retained their types
until the same `prepare_image` body. Public APIs, callers, loop, reservation and publication
were unchanged. Candidate7ccce649 passed clippy,27 units and5 doctests; truncating the loop to
one intent failed the existing seven-prior oracle. No tests or fixtures were changed.
Against7ea423ad, all six instruction counts increased: missing89768→90920,
replacement116744→118152, split29579→29597, tail450058→452810,
LARA total2014198→2014318 and named2013237→2013357. Map/LARA Wasm grew945/510B;
heap/stable page deltas stayed zero. The unchanged non-regression requirement rejects this
candidate despite functional success and split still meeting its original ceiling.
In the map Wasm, preparation was folded into `scalar_edit`, with calls to `reserve_scratch`
and `apply_edit`; the static scalar frame decreased208→192B. These codegen changes did not
produce a net instruction reduction and do not isolate the cost of length handling.
The candidate remains archived, not implemented. At the0349 close, lib.rs was restored byte-for-byte
to7ea423ad (commit4062d5d50), including the slice-only signature; acceptance at that checkpoint was1/6.
Source/artifact bindings, raw CSVs, mutant, restoration and inspection are under
`/Users/yota/.local/share/gleaph/evidence/adr0097/prepare-input-0349/`.

**Experimental: cover descriptor view reuse (0348 checkpoint; original acceptance then1/6).**
`EditImage::cover` obtains its final descriptor view from the existing grow/non-grow
conditional. Growth resizes before moving the suffix and reuses that returned view;
equal/shrink moves the still-live suffix before resizing. Common descriptor assignment
and offset rebasing remain single-owned. The unconditional second resize is removed;
Scratch capacity checks, packed bytes, gap-fill order, zero-count/contained behavior,
reservation and publication are unchanged. No helper, API, flag or storage change is added.
Plan0348 extends the existing bridge fixture with an equal-count left extension, exact
window/packed bytes and trailing rebase. It adds no constructor or test function. The
characterization passes old and new production; omitting the shrink resize fails with
four descriptors instead of two. Source7ea423ad passed clippy,27 units and5 doctests.
The bound map Wasm confirms the growing descriptor move now proceeds directly to the
final descriptor update rather than repeating capacity/resize dispatch. This is scoped
codegen evidence, not an isolated dynamic instruction price.
Against46dfcf02, replacement118664→116744 (−1.62%), tail459658→450058 (−2.09%),
split29843→29579, LARA total2017834→2014198 and named2016873→2013237; missing stays89768.
The improvement/non-regression hypothesis passes. Heap/stable page deltas remain zero;
map/LARA Wasm sizes increase134/216B. Only split satisfies its original ceiling; the other
five remain unmet. The LARA control retains the same pending admission changes and is
not clean-commit integration evidence. Source/build bindings, CSVs, native logs, mutant
and codegen evidence are under
`/Users/yota/.local/share/gleaph/evidence/adr0097/cover-view-0348/`.

**Experimental: split right bulk copy (measured46dfcf02 checkpoint; split ceiling met).**
`EditPlan::split_page` replaces its right per-entry redistribution with one prospective
source snapshot through `EditImage::read`. The split-local array holds at most528B,
derived from page capacity and entry width; the read covers exactly32 or33 old entries.
For a left insertion it writes one right chunk; for a right insertion it writes the
prefix/suffix around a new pair encoded by `set_entry`. Earlier staged values and newly
minted unbacked sources remain governed by the existing reader. No packed offsets or
borrowed windows escape their owner, and the snapshot is consumed before the left move.
Both pre-covers, reserve/admission, the non-inlined split boundary, metadata/link/directory
updates, physical layout and consuming publication are unchanged. This adds no heap
reservation, shared cache, codec or copy API. The528B temporary is not a peak-stack claim.
Plan0347 extends the split matrix with right-interior positions33/63 and staged values
on both sides of the insertion. Its14 cases passed old and new implementations; deliberately
ignoring staged source bytes or omitting the right suffix both failed the contents oracle.
Source46dfcf02 passed scoped clippy,27 unit tests,5 doctests and both source-bound IC runs.
Against c2e5c751, split measured46538→29843 instructions (35.87% reduction), below its
original30930 ceiling. Missing/replace/tail stayed89768/118664/459658; LARA total/named
stayed2017834/2016873. Heap/stable page deltas remain zero. The split-improvement and
non-regression hypothesis passes; **original acceptance is1/6, with five gates still unmet**.
The first source38b1d7ae remains a failed non-regression attempt: its dynamic read-width
expression caused a common-reader codegen regression. The final width selects the already
validated array capacity or capacity minus one entry (528/512), preserving exact source
bounds without changing the reader or its empty-read contract. Both attempts and bindings
are under `/Users/yota/.local/share/gleaph/evidence/adr0097/split-right-bulk-0347/`.

**Experimental: split left-window move (measured c2e5c751 checkpoint).**
`EditPlan::split_page` retains both pre-covers and consumes right-half sources before
changing the left half. If the insertion falls in the left half, it reacquires that
covered byte window, shifts the affected encoded entries with `copy_within`, and uses
`set_entry` for the new pair. A right-half insertion needs no left-entry rewrite. No
borrowed window survives another image mutation; `cover` remains the owner of packed
relocation and staged-byte preservation. Scratch admission, dirty-range publication,
metadata checks and physical layout are unchanged. The existing split owner is marked
`#[inline(never)]` to keep its temporaries out of the common preparation frame.
Plan0346 covers positions0/1/31/32/64 with clean and previously edited images; the
characterization passed the old implementation and rejected a deliberate no-shift mutant.
Sourcec2e5c751 passed scoped clippy,27 unit tests,5 doctests and both source-bound IC runs.
Against source85a3 below, missing/replace/split/tail measured89768/118664/46538/459658;
LARA total/named measured2017834/2016873. All six improve; split falls31.21%. Heap/stable
page deltas remain zero, but **all six original ceilings still fail**. This validates the
slice's split-improvement/non-regression hypothesis, not promotion or a cost decomposition.
The first source615fc80d result remains rejected: split46643 improved, but three basic
rows regressed by2 instructions per scalar call. Its common preparation frame grew
144→160B; the refined split call boundary restores144B. These are compiled-frame
observations, not peak-stack measurements. Both epochs and source/artifact bindings are
preserved under `/Users/yota/.local/share/gleaph/evidence/adr0097/split-left-window-0346/`.

**Experimental: edit-scoped lookup readers (measured85a3d1a8 checkpoint).**
`apply_edit` selects a byte-reader callback once per intent and delegates selection and validation
to one private `EditPlan::lookup`. Direct backing reads require an empty staged range set and
`backed_end >= PAGES_START`; otherwise reads use the existing `EditImage::read`. The same reader
supplies directory length, full16-byte directory entries, selected-page metadata, first-key equality
and in-page keys. `dir_len` still owns its cap check; `page` still owns checked offsets, extent/count
checks and link bounds in their original order, using its initial count snapshot when no writes are
staged. Mutation callers of these helpers always supply the prospective reader. Prior-value reads,
observer points, mutation branches, scratch/admission and publication remain unchanged. There is
no second metadata validator or new reader mode. The selection record is local to one dispatch,
not a cache or another editor; its ordinal is test-only. Plan0345 adds native coverage for metadata
requests in both an empty and a staged image. Source85a3d1a8 passed scoped clippy,26 unit tests
and5 doctests after a test-callsite correction, then both source-bound Wasm builds and IC runs.
With the unchanged fixtures, missing/replace/split/tail measured89896/119944/67657/464458
instructions; LARA total/named measured2018134/2017173. All six improve against their controls
(and cef3), but **all six original ceilings still fail**. The map control is830fc671 with the same
bench e63a23bd; LARA retains its separately bound lib515b control. These results support this
reader-routing change, not a component-cost decomposition or acceptance waiver. Raw CSVs and
source/build bindings are under
`/Users/yota/.local/share/gleaph/evidence/adr0097/lookup-metadata-0345/`.

**Experimental interface: borrowed write windows (introduced in plan0343).** Private `Scratch::resize_reserved`
returns the exact resized view and private `EditImage::cover` returns exactly the requested
`&mut [u8]` window instead of a packed offset; `write`, `insert_directory` and `remove_directory` borrow that window for their
existing copy work; the split pre-covers receive their window and discard it at the statement
boundary, and every subsequent write takes a fresh window. P2's normative dirty-byte,
reserve, safety and exclusive-publication contract above is unchanged: storage layout, packed
representation, descriptor coalescing, gap-fill intervals/order, codec and lookup routes are
untouched by that interface change, and private offsets are simply no longer the caller interface.
Plan0343 originally deferred validation; later source-bound lookup/split epochs are reported
separately above. Those measurements do not establish an isolated borrowed-window speedup.

Historical cef3 host-native evidence, not the present candidate: source `cef3f01da605bb8955ef404018f66cd9673c4634e5cbf472e8ad713621fb01ff` passed default-feature
host-native clippy (`--lib --tests`, warnings denied), all24 unit tests and5 doctests (one executable
example plus four ownership compile-fail cases). The separately approved cold run used temporary
archived-f5 lock bytes, restored original lock15, and preserved all125 input pins and absences;
7.608s elapsed is native execution evidence, not IC instruction pricing. The source-bound receipt
and full logs are in
`/Users/yota/.local/share/gleaph/evidence/adr0097/lookup-native-cef3-seq288-user/`.
The earlier022f attempt remains a separate, failed clippy epoch. cef3 candidate canbench-feature compilation, candidate Wasm and IC measurement HAVE run and are
separately bound: `/Users/yota/.local/share/gleaph/evidence/adr0097/direct-canbench-cef3-seq426-user/`
(receipt `8e04613c…`, six-row table `e0e840af…`) measured all six rows with **all six original
ceilings FAIL**, and those IC results are historical cef3, not the present candidate. Retained e545 source and Wasm
`835d503b…` are another IC-unmeasured epoch, not a build of cef3. All six original ceilings remain
binding: they failed for historical R and subsequently for measured cef3; the normative contract
above and acceptance block below are unchanged.

**Map execution status: acceptance BLOCKED (historical `9436291c…` endpoint).** Corrected map SHA
`9436291caf0d7c2e878839eecf986ac7246236ba3410d8a2dc1fa08ab43e6b71` uses one `apply_edit`, caller-owned
image preparation and consuming public publication, with sorted/coalesced dirty ranges and fallible
inline overflow. It reserves full minted-page backing before ready. The unchanged17 map tests,
27 frozen FreeSpan tests and5 doctests pass; both meaningful mutations reach their intended runtime
oracles and are restored exactly. The observer still verifies actual staged bytes before edit1's
selected-page rejection. Both reservation guards, all fixtures and benchmark source remain intact.

Every original IC gate still fails. Replacement is77,576→167,624 instructions, missing removal
85,672→158,312, split/directory30,930→155,448 and LARA1,527,576→2,256,530. These improve against the
blocked `be81cd30…` image but not against the approved-admission ceilings. Native traffic, zero page
growth and an aggregate total cannot waive any phase. The full comparison against both immutable
references, source/build provenance and closed budget belong in the
[correction checkpoint](../evidence/adr0097-demotion-design.md#caller-owned-image-correction-checkpoint).
No individual check/copy/allocation cost has been isolated and no necessary-cost waiver is established.

`PageHeader::decode/encode` now owns the four former header mappings; no header-layout duplication
remains at those sites. Draft0340 completes `shared-images` but leaves `bounded-proof` pending for
the failed cost gates. The one authorized correction is frozen; no second patch, broader benchmark,
commit, merge or further execution follows from its functional passes.

##### Proposed correction within the map owner

**Implemented and measured under authorization `098fb465`; acceptance still BLOCKED.** The
proposal below was executed once from plan `c5288c39…`, following the user-conveyed REQUEST CHANGES
review of `ca1c4b59`. Earlier snapshots, refusals and ledgers remain immutable. The
[report](../evidence/adr0097-demotion-design.md#caller-owned-image-correction-checkpoint) distinguishes
its actual improvement from the original acceptance ceilings that remain unmet.

The correction uses **caller-owned image preparation plus canonical `PageHeader` encoding/decoding**.
The existing `EditImage`, `EditPlan` and header type own the data; no type, module, cache, persistent
state, dependency, mode flag, `Pin`, heap box or unsafe storage was added. It removes image-value
transfers from the common call path, not through a scalar semantic fast path.

- `scalar_edit` and `prepare_edits` each initialize one local image. Private `prepare_image`
  fills `&mut EditImage` and returns `Result<(), FailedEdit>`; `EditPlan::new`/its image field
  borrow that image for planning instead of owning and returning it. Keep one `apply_edit`, the
  ordered loop, result reservation/push, test observer and final `admit_image`. On error the caller
  drops private scratch; no ready value escapes. No partially prepared image becomes public.
- Make private `publish_image` read `&EditImage`. `PreparedEdits::publish(self)` still consumes the
  exclusive owner-bound capability and calls that publisher; scalar publication remains inside its
  closed synchronous facade. Update the test-only direct split adapter to the same image lifetime.
  Delete the `Result<EditImage, FailedEdit>` return, `Ok(plan.image)`, owned `EditPlan.image` and
  by-value private publisher handoff. Public ready construction may still move the completed image;
  Rust/compiler move elision or a fixed stack size is not a contract. No custom cleanup `Drop`.
- Put `decode`/`encode` on the existing private `PageHeader`, using `PAGE_HEADER_SIZE` and the
  existing `PAGE_OFFSET_*` constants. `EditPlan::page` retains its one32B overlay read and validation
  order, then decodes; `set_page` encodes before staging. Canonical `read_page_header` reads one32B
  header and uses the same decoder; test-only `write_page_header` uses the same encoder. Delete all
  four independent field mappings and the now-unused `read_u16`/`write_u8`/`write_u16` helpers after
  verifying their callers. Encoding keeps the existing zero padding; basic edits still write only
  changed fields. This deliberately changes the canonical reader from5calls/27B to1call/32B,
  not the prospective reader. Record that source-derived traffic delta and price the unchanged
  LARA control; do not hide it as cost-free normalization or infer a component-level IC saving.

Preserve exact typed errors and scalar projections; result scratch still precedes ordered intents.
Keep selected-page corruption checks at their current point, including edit1 after edit0 staging;
retain checked page arithmetic, free-page/link checks, DirectoryFull/membership precedence and full
allocated-page backing admission with its responsible edit index. The public exclusive borrow,
no-destination single consume, and nonfallible/nonallocating publication remain unchanged. Pure
header decoding neither validates another page nor replaces admission. Independently reopened/raw
backing aliases remain outside the exclusive-region contract.

This is one private lifetime/codec correction, with no new public signatures and no new signature
exceeding four meaningful parameters. Encapsulation and dependency direction stay in the map;
header consistency improves by deleting duplicated knowledge. Fitness is deliberately bounded:
`Scratch`, `EditImage::read/write/cover`, `apply_edit`, split loops and directory shifts retain their
algorithms. Repeated split-range normalization and repeated admission/search work are separate
hypotheses, **not** permission to pre-cover payloads, remove guards, add blanket inline attributes
or tune container representation in this patch. The one unchanged scalar run and retained LARA
control now measure the joint correction, without a rebuilt baseline. The matched scalar Wasm no
longer contains the identified image-copy sequence; missing removal improves242856→158312. This
supports a contribution, not isolated copy/codec/compiler pricing. All original IC ceilings remain
binding and unmet. No second candidate, tuning campaign or use of unused ledger time is authorized.

##### Proposed fully-backed disjoint reads

**Experimental: implemented once; engine acceptance remains blocked.** Following approval of the
source-only report `01b6d0dd`/plan `ecb7968e…`, the user authorized exactly one diagnostic candidate.
Source `5da46e45…` implements this case inside existing **`EditImage::read`**, with no other production
change. The [execution checkpoint](../evidence/adr0097-demotion-design.md#shared-reader-execution-checkpoint)
records18/27/5 passing tests/docs, four restored runtime mutation failures and lower measured cost,
but every original IC ceiling still fails. `shared-images` stays complete; `bounded-proof` stays
pending. No further execution or tuning is authorized. The prior
[read-path diagnosis](../evidence/adr0097-demotion-design.md#remaining-prospective-read-path) and
reviewed plan snapshot remain preserved.

**Invariant and exact surface.** That helper alone composes canonical backing with pending bytes.
Retain its signature, `EditPlan::read` delegation/zero-initialized arrays, `Scratch` and all caller
checks. After the existing one `partition_point(r.end() <= address)`, reuse its first candidate for
both the new case and existing full-coverage case. For the existing request interval `[a,e)`:

- Direct canonical read is eligible only when **`a <= e <= backed_end`** and the first candidate is
  absent or starts **at/after `e`**. The nonwrapping comparison must not admit a wrapped end. Use
  the existing interval calculation; do not introduce a different error/overflow projection.
- In that case, read the entire nonempty destination once from `Memory`, then return. A zero-length
  destination performs no `Memory` read. No `Memory::size` call, cached flag, scalar/batch discriminator
  or use of `required_end` as backing. `backed_end` remains the actual backing snapshot.
- Otherwise retain the existing contained-range copy and backing-prefix/zero-tail/overlay code,
  unchanged. No second search, range list, helper, public API, parameter, unsafe code, `Box`, `Pin`,
  inlining attribute or new state. Only eligible reads cease visiting the general clamp/tail path;
  it remains the single implementation for reads that need it.

This is valid for an empty image **and** a nonempty disjoint image. Every range skipped by the
partition ends at/before `a`; if the first remaining range starts at/after `e`, sorted disjoint
ranges guarantee that no later range intersects the request. Full backing makes the old clamped
length equal `dst.len()` and its zero tail empty; disjointness makes every overlay copy unnecessary.
Thus the bytes and backing traffic are identical, including zero-length no-read behavior. The test
is about the entire interval, not just its first address. It applies equally during scalar and batch
preparation.

| Pending/backing relationship | Required result and routing |
| --- | --- |
| Fully backed, no overlap; including left/right adjacency and a gap between ranges | One unchanged canonical read; skip clamp, tail-fill checks and overlay-tail dispatch |
| One staged range covers the whole request | Staged bytes, no canonical read, including when the address is beyond backing |
| Partial overlap at either end or several separated overlaps | Existing backing-prefix/zero-tail initialization, then every intersecting staged range overrides it |
| No overlap but partially/wholly beyond backing within planned new-page extent | Existing bounded prefix read and zero padding; never send the whole request to backing |
| Staged bytes intersect that unbacked suffix | Zero only unstaged padding; staged bytes win, never substitute all zeros |

Do not move or bypass `EditPlan::page`'s checked offset/full-page end, allocated-page/link bounds,
old-page backing check, selected ACTIVE/length/self-link/free-link/minimum checks, or split/free-page
admission. Keep ordered error precedence, result reservation, final full-allocated-extent grow and
responsible index3968. Padding is an internal view for planned new pages, not permission to treat
an invalid old page as empty. Public capability ownership/publication/discard do not change.

**Implemented additional oracle.** `edit_image_read_partition` was appended before the production
change; every existing17 test body/helper, five doctests and benchmark remains byte-identical.
Its33 named cases share one existing `WorkMemory` with real three-page backing (`B=196608`), five
private images and no map/Graph/PocketIC constructor or new Memory type. Page63 starts at `B` by
owner geometry; its planned end is `B + PAGE_STRIDE`. This is a helper-contract fixture, not a public
map/admission witness or a formerly failing correctness case. Seed each checked short window with position-distinct nonzero backing bytes and
a distinct staged pattern; poison output buffers before reading. Expected bytes come from those
inputs, not this reader's output or range classifier. Assert bytes first, then exact read calls/bytes,
no writes/grows and unchanged backing, excluding setup/snapshot traffic. Stage through existing
reservation/write operations.

Cover an empty inline image, then nonempty disjoint reads on both inline and heap storage, both
boundary adjacencies, full staged containment, partial overlap from each side and multiple overlaps
with canonical gaps.
Force actual heap use with four disjoint staged ranges and more than32 encoded bytes; assert both
storage representations before their cases. Include a contained subread of a larger staged range
(nonzero packed source offset). Full containment must issue zero backing reads; fully-backed partial
reads retain one full-length backing read. Zero-length reads issue none.

For unbacked cases, a fresh empty image reading `[B+16,B+24)` must replace poison with eight zeros
without a backing read; fully staging that interval must instead return staged bytes, also without
a read. A straddling `[B-4,B+8)` request must read only four backed bytes. With backed prefix
`21 22 23 24` and staged `[B-2,B+2)=a1 a2 a3 a4`, `[B+4,B+6)=b1 b2`, expect exactly
`21 22 a1 a2 a3 a4 00 00 b1 b2 00 00` (hex). Also check the same straddle with no staged ranges.
These byte intervals deliberately exercise the private helper; they are not claimed as an observed
public aligned8/16/32-byte request. Existing growth tests do not assert this padding matrix.

Both existing mutation oracles still fail: early publication changes backing on late `DirectoryFull`,
and deferred backing violates the actual grow-before-ready assertion. The two additional, separate
mutations also fail at runtime: omitting disjointness reaches `inline-contained`, returning
`[19,20,21,22]` instead of `[162,163,164,165]`; omitting only `end <= backed_end` reaches
`unbacked-unstaged` and `vec_mem.rs:36`'s **`read: out of bounds`**. Its later byte/traffic assertions
and subsequent cases are not credited. All four restore exactly to `5da46e45…`. The resource
mutation likewise does not reach its deferred publisher or later assertions.

**Fitness and risk.** One interval relationship stays encapsulated at its existing owner; canonical
and prospective layout/admission definitions are not duplicated. Dependency direction and public
states do not change; no migration is needed. The branch adds eligibility work to overlapping reads
and might not lower IC cost. Keep call boundaries, caller initialization and all admission as
controls. More invasive helper extraction/const-generic specialization or call-boundary inlining
would add a second experimental variable and is not selected. A scalar-only reader/cache is rejected.
The measured diagnostic predictions hold: missing158312→138600 and replacement167624→153288,
with identical native traffic/scratch. Saved Wasm shows the eligible route reaching a full-length
stable read and return without the general clamp/tail path. Search, scalar and split caller bodies
are byte-identical to9436; scalar adjustment remains304B, not peak stack. This prices only the joint
compiled guard, not individual checks/reads. Missing still exceeds85672 by61.780%; replacement
exceeds77576 by97.597%. Tail pairs, split and both LARA scopes also fail their original ceilings.
The [closed schedule](../../plans/0340-prepared-paged-map-edits.md#proposed-read-case-validation--not-authorized)
used222.562308/600s, including50.341531/120s final use;160s was unassigned and unused. Split's
per-entry write/cover normalization remains a separate, unpriced hypothesis; this correction neither
repairs nor explains its residual. Full A0/A1 and landing remain unapproved.

##### Bounded component measurement — revised decision after review515d9868

**Superseded draft preserved; four premises withdrawn.** (i) The single guard's joint phase
deltas (split −4,908 among them) do not upper-bound remaining reader/canonical-routing cost,
and native read counts do not bound instructions — the reader share is unpriced, not ≤4,908;
(ii) ≥8,448 modeled copy bytes cannot yield a ≤~5% share of a516,425-instruction residual — a
byte lower bound is no cost bound; codegen/control/normalization stay unpriced and the
identity-copy question stays open, not rejected; (iii) `scalar_edit` creates a fresh
`EditImage` per public call (lib.rs1328–1334) and the fixture performs separate scalar calls
(bench.rs phases149/156/162/169), so no cross-call dirty-slot staging story survives —
per-call fresh-range traces only, canonical bytes distinct from staged bytes, tail's +268,175
unattributable; replace−missing/≈356 (different lookup routes) and missing+52,928 (one
no-write operation's excess) are not write-cost bounds or cross-phase floors; (iv) present
zero-init/container/call implementations are not proven mandatory — only
validation/arithmetic/error ordering are fixed; the evidence simply cannot rank components.

**One fully specified diagnostic measurement** — mechanism chosen now from pinned
canbench-rs0.7.0: cfg-static accumulators over `ic_cdk::api::performance_counter(0)` (ic-cdk
enabled by the map's `canbench` feature; canbench-rs BenchScope rejected for categories:
private reset, global per-name aggregation, Drop bookkeeping charged to enclosing intervals,
cross-name nesting double-count). Marks at `scalar_edit` (M0–M4, M1–M3 shared)/`EditImage::read`/`EditImage::cover`/`admit_image`
only — v2 read-wrap/write-excl sub-intervals dropped; snapshot+reset between the four phase
blocks excluding seeding/sanity; calibration is observation-only (three matched shapes,
K=64/16/32, min/mean/max with defined timestamps/reset/accumulator/anti-elision) —
empirical, NOT bounds: raw intervals with no certified E-bounded ranking ('cannot rank'
acceptable), the saved-row Δ kept only as reject-only empirical sanity (cancellation
possible), remainders definitional; fail-closed = exact match of the pre-registered WEIGHTED
per-kind count table (tail cover=64×3+64×2=320; quotients forbidden; pairs/visits/physical
endpoints distinct with M1–M3 shared; finalize-once at every early-return site without
changing errors/control flow) in the
[protocol revision](../evidence/sources/adr0097-map-read-decision-protocol-revision.json);
cfg-on semantic check = the fixture's own pre/post asserts + `validate()` in the
same instrumented wasm; source audit = all added hunks cfg-gated and cfg-stripped source
equal to `5da46e45…`. LARA exclusion is configuration-scoped: the scheduled feature set does
not feature-enable the map dependency and no LARA canbench build is scheduled — Cargo feature
unification or explicit forwarding could enable it in another build, so no absolute claim; no
LARA run. The corrected protocol — weighted count table, early-return finalize-once contract,
observation-only calibration, and fresh ledger (maximum600 = pre-final215 including all FOUR
mutation oracles + protected final120 + explicitly unassigned265, source-at-step and
native+Wasm capture rules) — is in the
[protocol revision](../evidence/sources/adr0097-map-read-decision-protocol-revision.json),
superseding those details of the
[v2 receipt](../evidence/sources/adr0097-map-read-decision-analysis-v2.json); withdrawals and
the P3 count clarification are in
[revision provenance](../evidence/sources/adr0097-map-read-decision-revision.json).
Instrumented components rank current-source candidates only — no per-component
admission-regression attribution, no unperturbed-cost reconstruction, no ceiling, no budget,
no execution.

**Execution endpoint and protocol/evidence refusal (review739d119d).** The schedule executed
under review dfaed91c, but its protocol outcome is **INCOMPLETE/non-compliant**: two driver
gate failures (setup `FileNotFoundError`; map archive/finally tail) were followed by
unauthorized repair/reactivation contrary to the stop rule — the "none of the enumerated stop
categories occurred" justification is withdrawn; restoration and spare capacity never
authorized continuation, and no later allowance retroactively validates the overrun. Real
observations retained: initial+final **18** map tests (**33** cases), **5** doctests (×2),
**27** FreeSpan, four runtime-red mutations with exact messages and M-restoration, cfg-on
fixture assertions passing (canbench exit0), `clippy --features canbench` exit0, and the
exact count/calibration oracle (endpoints1600/1856/4480/343; read512/704/1472/94;
cover64/0/320/74; siblings64/64/128/1; calibration64/16/32/1, delta4542). Endpoint:
formatted **M = lib `2c76e95637493bda72af9913cf237f5a0089bc9723731d440eb193f0e676cf89` /
bench `363d1d65e70c94c673db1d71d415ad9f009a3e5713a014ce0192c9cd527c6378`** (frozen originals
`5da46e45…`/`2f95a494…` preserved; M_draft receipts byte-preserved). Evidence gaps bound the
verdict: the sealed manifest does not bind final `ledger.json` (`35247b4b…` recorded vs
`e42fcc94…` actual; external coverage issued, old manifest/seal untouched); the initial map
executable lacks a contemporaneous archive (only final-map's `663e4a81…` is contemporaneous
for its own invocation; no rebuild offered); the saved-row sanity gate was never implemented
(observation-only, no post-hoc cutoff); `components.csv` is3-vs4-column (rectangular
re-export issued). Definitional remainders (replace154624/35648/246592;
missing211136/0/277952; tail441280/246592/735616; split33706/128248/65451;
loop/call/drop/recorder26626/25251/49697/893) are arithmetic on instrumented intervals, not
independent validation; with the200-instruction standalone calibration and saved-row deltas
up to +1,951,474, no calibration subtraction and no uninstrumented component ranking is
claimed. Recorded charges exactly **135.69941875094082**; correction/resealing work has no
recorded timing and is not asserted cap-compliant. Verdict/coverage/table:
[reconciliation](../evidence/sources/adr0097-map-read-reconcile-reconciliation.json),
[coverage](../evidence/sources/adr0097-map-read-reconcile-coverage.json),
[table](../evidence/sources/adr0097-map-read-reconcile-components.json). `bounded-proof`
pending; criteria5/6 unchecked; acceptance BLOCKED.

The complete later A0 source surface is `lara/edge/{span,free_span,edges,init}.rs`, minimal type
visibility in `lara/edge.rs`, and the map owner. It must include capacity/whole-header caller audit,
not just two new facade methods. No native tree, Graph, count-owner or A1 publisher is implemented
by the map slice. The [report](../evidence/adr0097-demotion-design.md#map-owner-execution-checkpoint)
identifies its blocked execution; the later owner work remains separately gated.

**Precise protocol trace** — analytical four-byte slab example, not a runtime witness or a claim
that today's placement selects these ranges. Start with 5 backing pages, published/cached
`C0=(5*65536-64)/4=81904`, canonical free `[16,32)`, and old owned ranges `[64,80)` and `[96,104)`.
The rest of the existing capacity retains its prior ownership. Metadata/placement owners determine
which old ranges can actually retire; free-space absence alone is not proof of ownership.

1. Enter `span_write`; begin a draft. Consume `acquire(12,None)`: choose `[16,28)` and retain
   `[28,32)` in the private free view. Canonical free bytes/records still describe `[16,32)`.
2. Consume `acquire(32,None)`: no fitting private free span; plan `[81904,81936)` and `C*=81936`.
   Neither capacity copy changes. The plan records both destinations and retains authority.
3. Consume `seal([64,80),[96,104))`: run all retirement/coalescing/index work against the view from
   steps 1–2, excluding both destinations. Prepare record/index backing, split scratch and final heap
   work. The slab owner ensures backing for `C*` using its unchanged four-page reserve policy:
   required 6 pages, reserve to 10, actual grow **5→10** in this example. That is not a capacity write.
   Final free view is `[28,32)`, `[64,80)`, `[96,104)` (28 slots). No canonical retirement yet.
4. A1 finishes every peer's encoded rows, pin/count transition, final requested rewrite and raw
   scratch. Force a rejection here, **after both destinations and complete retirement are ready**.
   Explicitly `ready.discard()`, then end the writer scope. It performs no release, growth, discovery
   or fallible cleanup. Capacity stays 81904, canonical free remains `[16,32)`, both old ranges stay
   owned and every row/count/pin is unchanged. Backing may remain 10 pages. Neither destination was
   canonically acquired, so no tail interval is inserted into the free list on cancellation.
5. Relax only that final-metadata fault. Re-enter the scope and repeat: same two starts, same free
   view and `C*`; existing backing suffices. Once the whole A1 object is ready, publish safe copies,
   all rows/pin/counts, then consume its `PreparedSpans` (capacity field/cache followed by final
   free/index/bin/header/heap image), retaining the outer authority through the remainder. Final
   capacity is 81936; both destinations are allocated, both old regions retired once and free is
   exactly the 28-slot set above. No compensation or second acquisition occurs at publication.

If any acquisition/seal step fails, its consuming error path discards the private draft instead;
logical postconditions are the same, although an earlier successful backing reservation may remain.
If a tentative placement is abandoned, discard the entire draft and plan anew under the same writer;
do not publish an intermediate acquisition or add per-destination undo. The zero-length request
returns `C*` without ownership or allocation. Positive avoid ranges are checked within `C*`; preserve
today's best-fit selection and tail fallback if that candidate overlaps the avoid range, but discard
its tentative edit rather than take/release canonically. Do not introduce an unreviewed alternate
free-span search. Best-fit's requested-bin scan is not unconditionally bounded by eight entries.

**Error order and first canonical write:** validate request/avoid slot bounds, then selected free
record and prospective edits in call order; check each retirement's range/destination exclusion and
existing duplicate/previous/next rules in order. Missing/overlap/corruption and address overflow stay
exact errors, tagged with acquisition or retirement index. Map errors retain their own edit index
and the free draft's corresponding caller-origin tag; unindexed batch scratch remains unindexed.
`SpanPrepareError` distinguishes these from slab growth, record growth, indexed map `PrepareError`
and scratch reservation. In particular
A1 must not route them through today's `GrowFailed { current_size: 0, delta: 0 }` facade projection.
Existing facade projections are a separate source-audited surface, not prepared-error semantics.

Map intents preserve order, repeated keys, missing-remove `None`, page recycling/drain/reset and
split/minimum/link rules. Earlier `DirectoryFull` cannot be rescued by a later remove. Preserve the
already-early split `DirectoryFull` then membership checks. Validate encountered geometry only;
no unrelated whole-tree/map validation promise. Scratch needed to represent an earlier intent can
fail before an unvisited later semantic error: the old r2 promise that *all* semantics precede *all*
resource reservation is superseded. Complete free-record planning and heap-image/maximum discovery,
then call the map's combined semantic/resource preparation. Remaining resource order is fixed:
**map → records → slab → warm-heap append capacity**, stopping with the actual site's error.
No map reads are needed after its ready image holds the exclusive borrow. Cold replacement-heap
and other scratch construction happen before seal completes. No error is reclassified as an
optional-capacity outcome by this protocol.

The first canonical write in an isolated `PreparedSpans::publish` is its changed capacity field, or
first encoded free-store write when capacity is unchanged. In A1 it is earlier: the first
source-overlapping payload copy, otherwise first row/pin/count publication (disjoint unused target
copies are not canonical). After that write, only prepared writes/copies and reserved heap operations
remain; no fallible metadata discovery, encoding, allocation, growth, coalescing, callback or fresh
planner invocation. Unexpected backing/alias/invariant failure is still a **nonrecoverable** boundary;
there is no native rollback or IC/Graph transaction claim. A bare address plus `release().expect`,
late epoch rejection or caught unwind does not meet this contract.

**Scratch and heap obligations:** let `A` be acquisitions, `R` retirements, `T` distinct changed
free records, `F/F'` initial/final active records, `H` actual old heap length and `C<=T` final changed
active candidates. The acquired-range list costs `O(A)`; record/bin/header images cost `48*T` plus
up to 128 bin heads and fixed headers. Map-owned encoded scratch is the actual dirty-range byte
count, plus range descriptors, result storage and actual overflow capacities. `1048*U + 16*J`
(`J<=8192`) is a coarse touched-page/directory envelope, **not** a per-edit allocation requirement;
fixed header fields are additional. The scalar inline and split bounds above govern materialization.
Do not snapshot untouched pages or all free records for routine planning. Streaming the canonical index with sorted overrides for a cold heap
costs `O(F log(T+1)+T)` plus heap construction; it is not `O(A+R)`.

A release-only batch can retain `max(old_largest, merged_lengths)`. A mixed acquisition/retirement
batch **cannot**: taking the largest span may shrink it. When warm and `H+C<=2*F'+64`, reserve append
capacity for `C` final candidates without changing old heap contents. Determine the final maximum
from changed candidates and a read-only best-first traversal of the existing heap's array, checking
candidate validity against prospective records. A temporary frontier uses the existing standard
`BinaryHeap` within this owner; it is not another allocator/cache. If `W` candidates are popped,
frontier work is `O((W+1) log(W+2))`, storage up to `min(H,W+1)` candidate/index pairs; `W<=H`, **not**
bounded by `A+R`. Prospective validity lookups add `O(W log(T+1))` work and up to `W` record reads;
a new candidate dominating the frontier permits early stop. Publication only appends into reserved
capacity and writes the precomputed summary; stale candidates remain subject to the
existing validity rule. No post-ready record validation or heap rebuilding is permitted.

When cold or beyond that stale threshold, prepare a replacement heap from final active records and
publish by swap. Account both old capacity and replacement `F'` capacity while preparing, plus the
warm frontier or `C` candidates as applicable (candidate is 16 bytes; frontier also stores an index).
Do not eagerly clone all `H`, rebuild on every warm scalar operation, or claim zero allocation from
zero page growth. Scalar release, take, pair and known-successor prefix restoration must share these
prospective rules; the prefix fast path retains its lookup advantage. Full-A0 review must measure
warm/cold visits, heap capacities and split scratch. The first map-only slice cannot certify this
heap algorithm, span authority, cancellation trace or A1's separate `Q`/`P` payload-scratch bounds.

#### Failure and state table

The boundary below is the **first canonical adjacency write**, not allocator reservation writes.
No-op results assume valid rows; encountered corruption must not become `Missing` or `None`.
Preparation may grow backing capacity; a rejected attempt need not shrink memory or erase unused
target bytes. It must preserve canonical roots/rows/counts/peers and return every newly owned span.

| Phase/outcome | Returned behavior | Canonical/ownership postcondition |
| --- | --- | --- |
| Missing/dead/wrong label | `Ok(None)` | No writes, allocation, conversion or events |
| Unsupported width, bad root, impossible count/length/address, or inconsistent source during preparation | Exact typed `Err` | Requested edge still live; source and peers unchanged; unpublished targets cleaned up |
| `S > 1024`, or an optional conversion reservation cannot be made | Policy unresolved; not an implemented success/retention rule | A spill-free target's limit is not proof of general unrepresentability; corruption never becomes capacity |
| Empty-retirement resources or bounded-work proof unavailable | Deletion remains gated; a synchronous candidate must reject pre-write | Last edge and every root/block remain owned; repeated rejection alone does not establish liveness |
| All preparation succeeds | Publish prepared deletion/conversion, then account/retire | Complete descriptor and physical cover; no partial value plane; each old resource retired once |
| Failure after first canonical write or failure of pre-reserved cleanup | No recoverable `Err`/best-effort success | Broken prepared invariant: abort execution; native unwind is **not** rollback |

Only failures **at known capacity acquisition sites** may be considered for a future approved
capacity disposition. Do not classify `CollectAllocationOverflow`, `LtbBlock`, invalid free spans or
malformed descriptors wholesale as capacity. The same outer error type can carry arithmetic or
corruption evidence. Expected failures must be handled before publication, not recast as invariant
panics. Post-publication operations require prepared ranges/resources, not debug assertions.

#### Empty-retirement liveness and work gate

`D = 0` does not bound retirement. Let `L_e = ceil(S/1024)` and, for a valid nonzero width `w <= 4096`,
`L_p = ceil(S/floor(4096/w))`. For a stream of depth `d <= 3`, its rooted block inventory is
`L + ceil(L/1024) + ... + ceil(L/1024^(d-1))`, subject to root cap 1024, u32 fields and available
minted blocks. Add edge/property inventories as `N`. Preparation must visit those `N` blocks and
retirement requires `N` releases; a flat u32 ID inventory alone needs `4*N` bytes, before
validation/scratch overhead. Block stride is 4112 bytes. A full value-selection scan can additionally
read `S*w` bytes; an empty-only inventory must not materialize dead values unnecessarily.

For `S = u32::MAX`, the width-0/8/33 format envelopes respectively give `N = 4,198,404`,
`12,595,212`, `38,869,098` blocks and minimum ID vectors of `16,793,616`, `50,380,848`,
`155,476,392` bytes. These are **source-bound arithmetic**, not constructible-fixture, platform-memory
or execution-limit claims; actual root-depth/pool/address limits also apply. Even the finite wire
limits are not a synchronous IC budget. The recorded 1024-position witness says nothing about them.

Before enabling last deletion, separately establish a bounded synchronous envelope under existing
limits, or approve bounded retirement using a reachable ownership/progress mechanism. No new cap,
leaked empty descriptor, permanent last-delete rejection, or new work/state format is authorized here.
A traversal stack bounded by depth/root fanout may reduce discovery scratch, but does not by itself
preserve a complete post-publication retirement inventory or detect duplicate ownership. Native
`Drop`/`catch_unwind` cannot prove cleanup after an IC trap. This gate remains open even if A0/A1 pass.

#### Results, counterpart resolution and events

The existing four public orientation wrappers keep their
`Result<Option<E>, DeferredBidirectionalLabeledError>` and
`Result<Option<EdgeRemoval<E>>, DeferredBidirectionalLabeledError>` return types. A no-move native
delete/conversion must actually preserve positions; it may not internally produce and discard a move
list. Physical root/prefix relocation produces no logical movement. A future movement-bearing explicit
operation must use a reviewed delivery surface rather than silently widening the overflow-shift result
or relying on final traversal. Descending queued requests and reverse reconciliation are affected consumers.

Existing **slab maintenance**, or separately approved native maintenance, may move survivors. Each `EdgeMoved` or
entry of `OverflowRewrite` is delivered once to the existing observer before continuation; Graph
uses `apply_edge_slot_moves` for removal results and `GraphSidecarMoveObserver` for maintenance,
not both for the same event. Forward movements rekey sidecars and inline/sidecar postings through
`relocate_edge_properties_for_move`; reverse movements have no canonical sidecars. Tests replay
only these events over independently seeded identities; `Finished` never resets the expected map.

Counterpart `PairOrdinal` is a temporary equal-neighbor live rank, not a stable identity. Resolve
the selected counterpart before changing either half; never recompute that rank from the already
deleted half. Current `with_graph_mut` only borrows `GRAPH`: there is no mate-cache invalidation
hook on this path, and `counterpart.rs` persists no mate index. Do not add one. Re-derive future
lookups from the changed rows; preserving positions prevents an already-resolved other-half handle
from being renumbered by conversion. Public one-orientation calls are not atomic paired deletes.

`DeleteVertexV1` has an existing in-flight operation/queue key. Its current `?` propagation after
pop is not a retry guarantee. Before exposing new recoverable pre-write rejections to that driver,
retain/requeue the original item and unchanged `removed_edges`, then stop the pass. A failure after
an earlier half committed must not be requeued as an untouched operation; the Graph message must
abort. Its matching-removal dependency remains GAP004, not implicitly repaired by this contract.
No new work tag, persistent progress index or GAP005 handler reuse is proposed.

#### Native versus IC containment

Native `VectorMemory` writes survive `catch_unwind`; the saved 513→512 panic demonstrates this,
not transactional rollback. Native tests prove typed pre-write rejection/cleanup and successful
state transitions. They must not catch a post-publication panic and keep using the graph as though
it were restored. Expected allocation failures are handled during preparation, not by panicking.

Graph's first stable write can precede LARA: index-build admission/sidecar removal happens before
`remove_*_with_move`. Its existing `trap_post_fence_commit` boundary must not return a recoverable
error after that fence. The IC [entry-point trap rule](https://docs.internetcomputer.org/references/ic-interface-spec/canister-interface/#interpretation-of-numbers)
discards that execution's state changes. This applies to one uninterrupted message, not earlier
messages, earlier `await` segments or another canister. PocketIC must prove the actual Graph wiring
rolls back adjacency, sidecars/postings/outbox and queue together; native unwind cannot establish it.
No new asynchronous boundary is introduced. The inspected lock pins CDK 0.20.1 and stable-structures
0.7.2; the official security skill's older CDK examples are not copied as version-specific evidence.

#### Alternatives and review gate

| Alternative | Benefit | Rejection/trade-off |
| --- | --- | --- |
| Permanent nonempty retention for `S > 1024` | Preserves positions with little conversion work | **Not selected or approved:** changes shrink behavior, fails `S=1025,D=512` despite a representable prefix/run target; idle reclamation absent |
| Position-preserving prefix/run conversion | Can retire native blocks for extents beyond a spill-free prefix, without changing positions | Must prove target/prefix ownership, independent dense values, acquisition and promotion behavior; finite target envelope and retained holes still leave larger-high-water reclamation open |
| Explicit `compact_vertex_edge_span_with_moves`-style operation | Existing result surface can carry real moves; at eligible degree at most `D` survivors need new positions | Current tree admission skips native trees and current wrapper without moves discards them; native packing, no-move caller exclusion, complete delivery on failure and `O(S)` scan work need separate approval |
| Extend existing one-step maintenance and observer delivery | Already has bucket/slot cursors, captured policy, persisted queue items and per-step `EdgeMoved`/bounded `OverflowRewrite` delivery; may reuse canonical progress without staged targets | Current producers/selectors are slab-oriented and tree admission skips; prove native mutation, enqueue/idle progress, cursor semantics/invalidation, source-scan bounds and sidecar delivery. A cursor alone does not bound an expanding tombstone scan |
| Stage an unpublished conversion across messages | Could bound individual preparation messages | Existing cursors do not own durable target reservations or a staged conversion; new progress/ownership state would need separate design, not an assumed prerequisite for every reclaiming alternative |
| Filter values only / ignore errors / hide moves in deletion | Small diff | Rejected: leaves mismatched values, unsafe cleanup or stale identities |

No reclaiming alternative is approved here. Existing movement surfaces do **not** prove native
reclamation, but they also prevent the claim that every safe reclamation design inherently requires
new persistent state. The smallest next candidate is A0's allocator preparation prerequisite, subject
to its interface/scope review; A1 whole-unit publication follows independently. Native conversion,
delete wiring, bounded empty retirement, Graph/PocketIC integration and the shrink-policy decision
remain separate gates. No layout/version, threshold, compatibility or migration change is authorized.

### Canonical point-state read prerequisite (2026-09-22)

`70b080ca0` implements only GAP-004's bounded topology/liveness read and dead/missing
admission contract, using the [existing LARA point owner](../storage/lara.md#canonical-point-reads).
Native tree positions are resolved through the existing roots, including property-bearing
buckets, without changing raw visitor semantics or the mutation assertion. Six real-backed
witnesses include both populated public orientations and a separate read-only directed
parallel-edge counterpart test. [Evidence and limitations](../evidence/adr0097-point-state-reads.md).

Matching mutation remains open under GAP-004. The canonical topology/no-op slice is independently
approved through `a80d2e1ac`; that approval does not cover general counterpart/Graph integration,
demotion, release/Wasm deletion or landing.

### Selected tree-property read prerequisite (2026-09-22)

`036649e67` implements GAP-006's bounded selected hydration through the existing selected reader
and native property-root resolver, using original logical positions rather than slab live ranks.
The [selected-read contract](0050-lara-traverse-read-api.md#7-selected-slot-reads-remain-a-first-class-capability)
and [source-pinned evidence](../evidence/adr0097-selected-tree-properties.md) distinguish live-only
selected output from tombstone-inclusive native bulk traversal. Read-only width8/33 boundaries,
partial edge leaf, exact root errors and no-property/slab/run controls are covered. No header,
threshold, layout, writer or demotion contract changed in that read slice. Independent review
approved its bounded read-only repair through `f297706ab`, not deletion, demotion, matching,
general atomicity/performance or landing.
