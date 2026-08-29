# 0088. Tree-CSR mode for high-degree label buckets (LTB block store)

Date: 2026-08-29
Status: accepted (2026-08-29; measurement gates and implementation slices
pending — implementation does not start until the gates in "Measurement
gates" pass)
Last revised: 2026-08-29

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
| Tree bucket span (no gap)                 | ≤ 2 × `R_max` = 2,048 slots (8 KiB) | gap-0 invariant + deepen                     |
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

- `w = 0`: no property root, no property blocks — zero cost (pay-as-you-go; an
  orientation with no tree buckets has an empty LTB store).
- `w > payload`: tree promotion is rejected (typed error); inline properties
  are compact traversal-critical values by contract, so this is a declared
  bound with a widening trigger, not a supported case.
- Width transitions stay **fail-closed exactly as today**
  (`InlinePropertyBytesWidthMismatch`); tree mode does not regress them. A
  future `materialize_inline_property_stream(bucket, w, fill)` primitive
  (reserve `ceil(S/K)` blocks; commit fill + root extension + one descriptor
  republish) is recorded as **planned**: the derived geometry makes
  later width addition an incremental, steppable operation instead of one
  `S × w` contiguous allocation. Value backfill semantics stay above LARA
  (ADR 0008 profile SSOT; ADR 0058/0059 flavor). `w1 → w2` re-encoding and
  `w → 0` teardown are separate deferred transitions.
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
| `crates/ic-stable-lara/README.md`                    | Tree mode summary                                                                            | on implementation |

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
