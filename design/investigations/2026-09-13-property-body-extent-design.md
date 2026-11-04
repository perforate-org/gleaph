# Property bodies: coalescing extents and capacity reuse

Status: Experimental — native and managed IC comparison complete; production integration unselected.

IC checkpoint: 2026-09-13 20:50:20 UTC +0000. The
[managed IC comparison](2026-09-13-property-body-extent-canbench.md) measures91 cases under one
region policy, including extra index capacity, slower point reads and fragmented-allocation costs.

Native checkpoint: 2026-09-13 14:02:33 UTC +0000. The
[native extent comparison](2026-09-13-property-body-extent-comparison.md) implements this probe after
the comparison-first revision of 2026-09-13 09:36:59 UTC +0000 and the subsequent B-tree investigations.
Its native results do not select a production winner or establish IC instructions/rollback.

Design anchor: 2026-09-13 08:26:10 UTC +0000.
Repository anchor: `95d7910878a5ee8af95af878f2b0df952edd70dc`.

## 1. Decision and evidence boundary

The native/IC candidate uses **one contiguous variable-size extent per external body**, immediate
coalescing of free neighbors, and reuse of an existing extent during replacement. It uses a
size-ordered, fixed-width stable free-space index rather than scanning free lists. The experiment
stays inside the prospective PropertyStore design; no general allocator crate or change to LARA's
allocator is authorized. The specification below describes the shared native/IC prototype, not an accepted
production layout.

The [property-value separation experiment](2026-09-13-property-value-separation-design.md#8-probe-results-and-disposition)
proved neighbor isolation but did not select its fixed-block/replacement-body candidate. That result
remains unchanged. This document owns the follow-up allocator format/rules; the native report owns
its measurements. Neither replaces the previous experiment's measured layout or reattributes its results.

Two independent costs need attention:

- Fixed blocks trade tail waste against selected-body link reads. A 65-byte body plus reference
  occupied 142–4110 logical bytes across the tested block sizes, before B-tree overhead.
- Allocating a whole replacement before freeing the old body retained roughly two large bodies'
  capacity in the single-hot-value experiment. That was not a leak or an unavoidable cost for every
  graph; it was a consequence of that replacement strategy.

Separation does not require replacement-body allocation for every update. In-place mutation can
be considered only after old values and every fallible requirement are prepared, under exclusive
local execution with no intervening read/mutation/await. A failed remote callback cannot undo an
already completed local segment; existing Graph/Router outcomes must still govern replay.

## 2. Alternatives and existing owners

| Approach | Advantage | Disposition and cost |
| --- | --- | --- |
| Fixed blocks plus in-place reuse only | Smallest change; reduces replacement high-water | Leaves the measured short-body waste / large-body link trade-off. Useful conceptual control, not a revival of the rejected layout. |
| A few segregated slab sizes plus large extents | Dense short values; fast class lookup | Adds class rounding, occupancy tracking, partial-page lists and cross-class page reclamation. Those costs would obscure the first extent/reuse comparison. |
| TLSF-style bitmap bins and boundary tags | Bounded metadata search, split/coalesce and resize patterns | Stable offsets, persistent bitmaps/list consistency and class mapping are another allocator implementation. Retain as an alternative if the proposed free-index cost fails. No TLSF port or dependency is selected. |
| Two free-space B-trees, by address and size | Straightforward neighbor and fit lookup | Duplicates the search metadata and co-write surface. Physical boundary tags can locate neighbors without an address-order index. |
| **Boundary tags plus one free-by-size B-tree** | Contiguous reads, fine allocation granularity, exact fit search and neighbor coalescing | **Native/IC comparison complete; not selected.** Pays one extra fixed-width index per arena and logarithmic metadata work; neither is free. |
| Capped body chunks in the existing B-tree | Reuses physical allocation/reclamation; bounds per-entry payload without capping whole values | [Measured first](2026-09-13-property-body-btree-comparison.md): node geometry affects density; per-chunk mutations amplify large-body writes. These costs do not establish an extent win. |
| Existing LARA `FreeSpanStore` unchanged | Already reclaims/coalesces physical ranges | `pick_span_in_bin` can continue past its initial eight records; `record_max_candidate`/`rebuild_max_heap` can walk the full index. It also owns LARA slot records and summaries. Not a request-bounded drop-in. |
| Adopt `ic-stable-memory 0.4.4` | Already provides allocation and in-place growth | Its allocator uses heap `BTreeMap<u64, Vec<FreeBlock>>` and serializes allocator state for upgrades. This changes persistence/upgrade ownership and is not the directly stable metadata model sought here. |

These are source-backed distinctions, not allocator speed rankings. Doug Lea's historical account
explains boundary tags and smallest-first coalescing; it explicitly does not describe every detail
of later dlmalloc versions. TLSF source demonstrates class-search rounding and split/merge resizing,
not a proof about IC instruction costs or a guarantee of exact best-fit.

## 3. Ownership and metadata source of truth

Production ownership remains the [existing vertex/edge PropertyStores](2026-09-13-property-value-separation-design.md#2-ownership-and-preserved-behavior):
private shared implementation, disjoint per-store body storage. No Router, index canister, GQL,
adjacency, physical edge ID or reference-retention change is proposed.

For this **allocator-only** probe:

- The caller supplies an opaque body descriptor `(header_offset: u64, encoded_len: u32)`.
  Encoded length remains caller/directory-owned. It is not stored again in the arena.
- Arena tags own **physical capacity and allocation state**, not encoded length. Capacity may
  exceed the body length after alignment or absorption of an unusably small remainder.
- One derived free index owns fit lookup, not canonical property data. Its membership must equal
  the Free spans in the arena partition. Arena header sizes are authoritative; index keys and
  trailing size tags are checked navigation/search metadata, updated by one allocator operation.
- No second by-address tree, free-byte counter, largest-span heap, live-object registry, length map,
  allocator journal, background rebuild or garbage collector.

The free index is `StableBTreeMap<FreeExtentKey, (), M>` using exact `ic-stable-structures 0.7.2`.
`FreeExtentKey` (the prototype uses the existing `[u8; 16]` Storable) is fixed 16 bytes: total span size u64 BE, then header offset u64 BE, ordered in that
same tuple order. Its Storable bound is fixed 16. The existing unit Storable encodes zero bytes but
has `Bounded { max_size: 0, is_fixed_size: false }` in 0.7.2; do not assume a fixed-size value layout
or omit upstream length/node overhead from accounting. It stores individual keys, not per-size vectors. Ties use the lowest offset. This is allocator
metadata, not the Property Index domain.

The allocator requires **two disjoint memories**: raw arena/header and free index. A future property
directory is a third collection/region, outside this probe. Do not colocate their offset-zero
headers. One implementation can later serve the two PropertyStores, but their memories/references
must never be mixed.

## 4. Experimental arena format

This is a probe format only. No MemoryIds, production threshold or property-cell codec are chosen.
It is not the fixed-block `PVB` format; do not interpret old block IDs as byte offsets.

| Arena header offset | Bytes | Meaning |
| ---: | ---: | --- |
| 0 | 3 | magic `PVE` |
| 3 | 1 | version 1 |
| 4 | 4 | zero reserved |
| 8 | 8 | logical arena end, u64 LE; initially 64 |
| 16 | 48 | zero reserved |

The interval `[64, logical_end)` is completely partitioned into nonoverlapping spans. The logical
end is monotonic; backing page rounding lies outside this partition and is not a free span yet.
Both ends and every span size are 8-byte aligned. Minimum span size is 32 bytes.

A span is `[8-byte header | capacity bytes | 8-byte trailing tag]`. The header is u64 LE:
`total_span_bytes | live_bit`, where bit 0 is Live=1 / Free=0, bits 1–2 are zero, and the remaining
bits encode the aligned total. The trailing tag is just that total, without the live bit. It must
match the header. Payload starts at `header_offset + 8`; capacity is `total - 16`.

For a positive body length b, compute with checked arithmetic:

`need(b) = max(32, align_up(b + 16, 8))`.

Retain `b + 1 <= u32::MAX` for prospective property compatibility; the probe rejects zero-length
arena allocations. Stored NULL, small-cell placement and Value validity remain directory/codec
concerns, not allocator rules. Validate offsets, alignment, minimum size, reserved bits, envelope,
header/trailer agreement and `b <= capacity` before relying on a descriptor.

A Free span has no encoded body length. Its payload need not be erased; it is inaccessible through
valid live descriptors. At completed operation boundaries there must be **no adjacent Free spans**.
That invariant limits coalescing to the immediate neighbors instead of an unbounded chain walk.

### Creation and reopen

Both memories must have size zero for creation, or both must be initialized for reopen. Refuse a
half-pair before any grow/write. For a new pair use the owning B-tree `new`; on reopen validate the
arena header then use the owning B-tree `load`, **not `init`**: 0.7.2 `init` creates a new map on bad
magic, whereas `load` checks it. Do not copy an upstream header decoder to implement this check.
Bad upstream metadata may trap; typed arena errors do not imply a universal typed corruption API.

Reopen does not walk all spans, deserialize a heap free map or repair an index. A full partition/
index equality audit is a test oracle only. Touched inconsistency fails closed; fabricated untouched
aliases or corrupt upstream B-tree lengths are not covered by a general corruption guarantee.

## 5. Allocation and replacement rules

### Allocate and release

1. Find the first free key at or above `(need(b), 0)` using a lower-bound range and one result.
   Validate that selected span against its canonical tags before consuming it. No free-list scan.
2. Allocate from its front. Split the remainder only if it is at least 32 bytes; otherwise absorb it
   into capacity. A split tail is immediately reusable, with one index entry.
3. If no fitting span exists, inspect the final physical span through its trailing tag. Extend an
   undersized trailing Free span by only the shortfall, or append after a trailing Live span.
   Never append while the fit lookup has already found usable space.
4. Release validates a Live span and coalesces its immediate Free left/right neighbors. Remove their
   old free keys once and publish one merged free key. A second release before reuse is invalid.

The left neighbor comes from the preceding trailing tag, corroborated by its header; the right
neighbor comes from the current total. Endpoint checks precede tag reads. No same-size linked list,
by-start index or scan for a distant coalescing partner.

### Replace, in priority order

Read and retain required previous bytes before overwriting them. Admit combined old/new encoded
bytes/output requirements before body reads or output-buffer allocation.

| Condition | Action |
| --- | --- |
| New body fits current capacity | Keep the address; split a tail of at least 32 bytes and merge that tail with a Free right neighbor. Absorb a smaller tail. |
| Immediate right Free span makes sufficient capacity | Consume it, keep the address and split/absorb the remainder by the same rule. |
| The current span, possibly followed by an insufficient Free span, reaches logical end | Extend that capacity by the shortfall and keep the address. |
| Otherwise | Allocate a separate fitting/grown span, write the new body, publish the new descriptor, then release the old span. |

Do not shift into a left neighbor for in-place growth, compact unrelated bodies, keep oversized
capacity indefinitely after a large shrink, or add a second resize policy. Relocation still has an
old+new peak. External fragmentation can still force growth while total free bytes look sufficient.

### Preflight and commit boundary

All deterministic validation and limit errors precede the first allocator/index/body write. A
single operation runs under exclusive local access; no reader, mutation or await may intervene.
Prepared geometry is private to that operation, not a retained address cursor or public transaction.

**Relocation must plan retirement against post-allocation neighbors.** A chosen free extent may be
the old body's left neighbor; splitting it changes the neighbor later released/coalesced. Reusing
the original neighbor snapshot could free the new body. Compute the allocation/intermediate spans,
then the retirement spans, deduplicating affected original spans and free keys by offset.

For one operation, the affected original spans are at most the old Live span, two Free neighbors
and the chosen free span (overlap deduplicated). A bounded local edit description is appropriate;
a generic memory-write overlay, second journal or whole-free-index clone is not. Prepare allocation
and post-publication retirement stages explicitly, as well as net index removals/insertions. Reject
any deterministic error before applying either stage. At most three original free keys are removed
and two resulting free keys inserted; this is not a claim of five physical B-tree writes.

After the first write, commit code must have no recoverable Err exit. Unexpected failures trap in
the future Graph local segment. New body data precedes a relocated directory reference, and old
retirement follows that publication. In-place replacement may temporarily disagree with the old
directory length inside the exclusive committing segment; publish the new length before returning.
Previous bytes needed by indexes must already be owned, not reread from overwritten storage.

The probe's caller descriptor update is an infallible test-owned action. It does **not** establish
atomic co-write with a persistent PropertyStore directory or Graph fence/outbox. Ordinary errors
from a later caller cannot be treated as rollback. Native panic-catching is not IC rollback proof.
Whole-row preflight, interacting allocations across rows/keys and local edge-slot remapping remain
separate integration obligations; individually prepared replacements do not satisfy them.

## 6. Work and space accounting

One selected body is one contiguous payload read, plus bounded-size arena/tag reads. Body admission
precedes those body-buffer requests; count metadata and payload separately. Release/resize touches
immediate neighbors, not all free spans. One lower-bound fit and a constant number of exact free-key
checks/inserts/removes have **O(log F)** tree work for F free spans under valid B-tree invariants.
The index never contains body bytes or variable-length per-size lists.

This is not constant-time allocation, a numerical IC instruction guarantee, or a total read-budget
API. Bound tree height/node packing and memory-manager work from valid cardinality and fixed-width
records before using it for Graph row admission. Do not invent a global property cap to hide that
missing analysis. The native probe measures F-scaling; it does not establish Graph directory work,
Value decoder allocations or B-tree write/rebalance admission.

For bodies of at least 33 bytes, fresh/split capacity uses b+16 plus at most seven alignment bytes.
An absorbed remainder adds at most 24 bytes. Including the previous experiment's illustrative
14-byte reference gives these **arithmetic estimates, not measurements or a chosen cell format**:

| b | Requested span | Span + illustrative reference | Existing b+1 envelope |
| ---: | ---: | ---: | ---: |
| 33 | 56 | 70 | 34 |
| 65 | 88 | 102 | 66 |
| 129 | 152 | 166 | 130 |
| 1048576 | 1048592 | 1048606 | 1048577 |

These exclude absorbed tails, directory nodes, free-index nodes, free/high-water extents and page
rounding. The encoded size of a free-index key is 16 bytes, **not its full physical cost**. Many
small unusable-in-isolation holes can make the index expensive; measure that adversarial case.

The proposal adds an index region beyond the rejected arena-only allocator. Two memories per arena,
for two PropertyStores, mean four added regions if adopted. Under the existing four-page default
backing policy that is conditionally 1 MiB total initial backing, not 512 KiB. No MemoryIds or
region policy are selected. Count native VectorMemory pages separately from this conditional IC
memory-manager cost. Freeing/coalescing does not shrink stable memory.

## 7. Native checkpoint and adoption gates

Plan 0352 was revised from the unimplemented extent-only draft to the
[B-tree comparison](2026-09-13-property-body-btree-comparison.md). Plans 0353/0355 then measured IC
instructions and wider chunks. After explicit user authorization, plan 0356 implemented the
[native extent comparison](2026-09-13-property-body-extent-comparison.md): six tests, 36 extent rows,
all 260 reference rows unchanged, paired reopen and full partition/index/owner oracles. It covers
equal-size reuse, split/absorb/coalesce, right-neighbor/frontier growth, post-allocation old-left
retirement, fragmentation, retained relocation capacity and pre-effect rejection. The private span
plan validates at most four original/three final spans; full scans remain test-only.

Native I/O improves large replacement/shrink and neighbor isolation, but adds a second raw-region
floor and allocator maintenance burden. Plan0357 then completed the91-case managed IC comparison:
large replacements/shrinks and neighbor updates improve, but Whole point-get and already-coalesced
fragmented insertion can be cheaper, and the extra region remains allocated. Both owners use the
existing variable manager over direct IC memory with a four-page experimental policy; native tests
keep their counted backend. This supports a separately reviewed integration design, not persistent-
directory atomicity, whole-row admission, update rollback or upgrade proof. No production integration
is automatically authorized.

Before production:
choose one codec/layout in an ADR, update ADR 0007 and inventory, preserve all Value/key/move semantics,
reuse existing previous-value preparation, establish directory/allocator/decoder/whole-row budgets,
and run real Graph canbench plus the owning normal PocketIC lifecycle/rollback/upgrade fixture.
Activation requires the [coherent fresh logical-graph reset](2026-09-13-property-value-separation-design.md#7-activation-and-next-decision),
not casual Graph-only reinstall. No compatibility branch or dependency fork is introduced here.

## 8. Sources and review disposition

- [LARA free-span owner](../../crates/ic-stable-lara/src/lara/edge/free_span.rs):
  `pick_span_in_bin`, `record_max_candidate`, `rebuild_max_heap`, and the owner-specific release path.
- [Installed 0.7.2 B-tree source](https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap.rs):
  `new` derives page size from bounded K/V; `init` can initialize on bad magic; `load` reads/checks it.
  The [unit Storable](https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/storable.rs)
  has max_size zero but is_fixed_size false.
- [Doug Lea, A Memory Allocator](https://gee.cs.oswego.edu/dl/html/malloc.html): historical boundary
  tags, smallest-first selection and wilderness discussion; not imported dlmalloc code or benchmarks.
- [TLSF implementation](https://raw.githubusercontent.com/mattconte/tlsf/master/tlsf.c): inspected
  `mapping_search`, `block_trim_used`, `tlsf_realloc` at the design anchor; mutable URL, no pinned
  dependency or port selected.
- [ic-stable-memory 0.4.4 allocator](https://docs.rs/ic-stable-memory/0.4.4/src/ic_stable_memory/mem/allocator.rs.html):
  heap free map, serialized allocator state, and `try_reallocate_in_place`.
- [IC stable-memory skill](https://skills.internetcomputer.org/.well-known/skills/stable-memory/SKILL.md)
  and [security skill](https://skills.internetcomputer.org/.well-known/skills/canister-security/SKILL.md):
  direct stable metadata, distinct regions, upgrade and message-local/callback failure boundaries.

Same-assistant disposition: **managed IC comparison complete; review integration before selection**.
Encapsulation and separation stay with the prospective existing stores; tag/index invariants have
one writer; duplicated physical search/navigation metadata has an explicit co-write contract.
Native correctness/I/O, IC instructions/region costs and implementation size are reported separately.
Fitness remains conditional on workload, fragmentation and production failure/whole-row contracts.
No independent or security review, IC update rollback/upgrade validation or production adoption is claimed.
