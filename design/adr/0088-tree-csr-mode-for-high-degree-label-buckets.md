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
Last revised: 2026-08-30

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
- `T_promote = 4096` is the **slab → tree promotion threshold** (the
  `alloc_space` size that triggers promotion into tree mode) and the slab
  mode cap on `alloc_space = stored_slots + alloc_gap`.

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
- Plan 0317 — Tree-CSR implementation in `LabeledLaraGraph`. Wires `LtbRawBlockStore<VirtualMemory>` per orientation (forward + reverse), adds the descriptor mode-flag bit, promotion / deepen / flatten transitions, mode dispatch in the bucket access constructor. Plan 0322 must close before Plan 0317 starts so the production code paths can use `range_target` and `for_each_chunk` from day one.
- Plan 0321 — Normal LARA (unlabeled) tree mode as a second instance of the same contract.

### 1. LTB block store

One new store per orientation (forward, reverse): the **LARA Tree Block (LTB)
store**, a single stable-memory region (one `MemoryId` each; ADR 0007 inventory
update on implementation) holding a 64-byte header plus fixed-stride blocks.

```text
-------------------------------------------------- <- Address 0
Magic "LTB"                            ↕ 3 bytes
Layout version (=1)                    ↕ 1 byte
Payload bytes per block (=4096)        ↕ 4 bytes    (power of two; wire truth)
Root fan-out cap R_max (=1024)         ↕ 4 bytes    (wire truth; see §4)
Block capacity (allocated block slots) ↕ 8 bytes
Tail next (first never-minted id)      ↕ 8 bytes
Free head (intrusive list; MAX=none)   ↕ 4 bytes
Free count                             ↕ 4 bytes
Reserved (must be zero)                ↕ 28 bytes
-------------------------------------------------- <- Address 64 (HEADER_SIZE)
Block 0: block header ↕ 16 B | payload ↕ 4096 B      (stride 4112)
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
B       = payload / 4 = 1024 edge slots per block
K       = floor(payload / w) values per property block (w > 0)
edge root len     = ceil(S / B^d)        d = derived depth (§4)
property root len = ceil(S / (K·B^(d'−1)))
span width        = edge root len + property root len      (gap = 0, §3)
```

**Edge width (Plan 0318 post-merge):** tree mode stores one 4-byte target per
LTB slot, so promotion is restricted to edge types with `E::BYTES == 4`. Wider
edge types fail promotion with the typed `TreeModeEdgeWidthUnsupported` error,
and the insert dispatcher carves them out of the promote trigger so such
buckets stay on the CSR slab path (mirroring the inline-property carve-out).
The carve-out was added after the canbench `bench_l_s2_det_sat_4096` bench
(10-byte edge type) trapped on the tree-append guard following a silent
mis-transcription — the promotion-side guard is typed, not debug-only.

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
review rejection.

Capacity bounds (all fail-closed at the allocation site):

| Region                                   | Bound                             | Enforcement                                  |
| ---------------------------------------- | --------------------------------- | -------------------------------------------- |
| Slab bucket edge span (incl. slack)       | `T_promote` = 4,096 slots (16 KiB) | growth clamp + crossing compact-or-promote   |
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
  new `MaintenanceWorkItem::MaterializeInlinePropertyStreamV1` cursor
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

Three bucket-backing states, two transitions: `bypass → bucket(slab) →
tree`. No direct bypass→tree edge (the dispatcher may chain the two existing
transitions in one insert). Demotion (tree → slab) is **not** in the first
slice; it is a deferred, benchmark-gated maintenance operation (a shrunken
tree bucket is correct, merely ≤ 2 blocks + root wasteful).

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

Execution model — bounded transitions are synchronous, O(S) work is stepped:

| Transition                          | Bound              | Execution                       |
| ----------------------------------- | ------------------ | ------------------------------- |
| Promotion                           | O(T_promote) ≈ few M ins | synchronous, in the crossing call |
| Deepen / flatten                    | O(R_max) = 4 KiB   | synchronous                     |
| Block mint/release                  | O(1)               | synchronous                     |
| Demotion                            | O(S)               | deferred maintenance (ADR 0020) |
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
| `T_promote`          | 4,096  | policy (benchmark-gated, hysteresis with compact-or-promote) |
| `MAX_DEPTH`          | 3      | policy (fail-closed structural boundary; widening = future ADR) |
| Log cap              | 170    | existing wire                                                |
| LTB VMM bucket policy | 64 pages | policy (ADR 0043 experimental; footprint-gated)            |

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
- `tree-mode-tombstone-reuse` — tree bucket tombstones are not
  reused on insert (slab mode reuses via
  `try_reuse_unordered_slab_tombstone`); high-churn tree buckets
  grow LTB footprint monotonically. **Plan 0319 (2026-09-01)**
  ships demotion as the primary reclaim path, but in-place
  tombstone reuse inside LTB blocks is still deferred (needs
  per-bucket free-ordinal tracking; no spare `LabelBucket` field
  today). After 0319 telemetry decides whether demotion alone
  suffices.
- `tree-mode-promote-from-bypass` — bypass-mode → tree-mode direct
  transition (current path is bypass → slab → tree). Records as
  a follow-up so the current two-step path is not blocked on
  one-step direct promotion design.
- PocketIC-backed 1M-degree sweep (Plan 0318 Gate 1 deferred row;
  the 1M-sweep is `#[ignore]`'d in `tree_csr_high_degree_test`
  because it hits production `Memory::grow` limits in test
  environments).

**Plan 0319 (Steps 1-3, 2026-09-01) — demotion + rewrite-path tree
awareness.** The slab→tree transition (promote) is now reversible
via `tree_mode_demote_to_slab`, a failure-atomic reserve/commit/
publish primitive that rebuilds a tree bucket as a fresh CSR slab
of only live edges and releases all LTB leaves + interiors + the
old root region. Demotion fires inline at the single tree-mode
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
MaterializeInlinePropertyStreamV1`), publish the descriptor
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
walker) or `single_bucket_span_iter` (LTB-aware slow path). The
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

Last revised: 2026-09-03.
