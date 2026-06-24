# 0032. Vector index slab page store

Date: 2026-06-24
Status: implemented
Last revised: 2026-06-24

> **Status note:** Implemented as of `2026-06-24 16:13:21 UTC +0000`. The composite slab page store
> (`VECTOR_PAGE_META` MemoryId 10 + raw `VECTOR_ROW_SLAB` MemoryId 13, opened together with
> fail-closed reopen validation) replaces the former `VECTOR_PAGE` large-value store, encapsulated
> behind `VectorSlabStore` in `crates/graph-vector-index/src/facade/stable/page_store.rs`. No
> old-page migration, compatibility reader, or canonical backfill/rebuild step was added; the new
> `VECTOR_INDEX_STABLE_LAYOUT` is defined directly (14 regions, 0–13). Benchmarks (`canbench`) show
> 30–95% instruction reductions across search / upsert / rebuild paths versus the prior Candid page
> layout; see `crates/graph-vector-index/canbench_results.yml`.

## Context

ADR 0031 makes graph shards the canonical owner of vertex embeddings and makes the
`graph-vector-index` canister a derived candidate-generation structure. The vector index canister
owns its internal vector rows, IVF partitions, rebuild state, and search scan mechanics; Router owns
definition resolution and query orchestration; graph shards remain the source of truth for canonical
embedding bytes.

Through ADR 0031 Slice 8, vector rows are stored in `VECTOR_PAGE`, keyed by
`(index_id, index_version, partition_id, page_id)`. The uncommitted Slice 9 storage experiment
replaces the earlier Candid `Vec<PageRow>` value with a raw fixed-stride page buffer. That removes
page-wide Candid decode/encode from row reads and appends and improves the vector-search benchmark
substantially.

That improvement still leaves the page body as a `BTreeMap` value. Appending a row, tombstoning a
row, and reading selected pages still operate at the stable-map value boundary rather than at the
physical byte-row boundary. Partition-page search also pays a reverse-locator lookup through
`VECTOR_ID_TO_SUBJECT` for every candidate row before re-validating freshness against
`VECTOR_SUBJECT_TO_ID`.

The relevant local precedent is the `ic-stable-*` storage family:

- `ic-stable-lara` keeps large edge and payload slabs in raw stable-memory regions and stores small
  metadata separately.
- `ic-stable-lara` composite stores reject partial reopen layouts rather than recreating missing
  regions and overwriting live state.
- `ic-stable-lara` traversal code uses contiguous slab reads and batch visitors for dense hot paths.
- `ic-stable-vec-deque` and `ic-stable-paged-ordered-map` use explicit magic/version headers and
  validate persisted layout invariants on reopen.

The vector-index page store should adopt those storage patterns without turning vector indexing into
a generic storage crate or moving vector-index invariants outside the vector-index canister.

## Problem

The existing `VECTOR_PAGE` value layout is no longer the right ownership boundary for vector rows.
It hides page internals from callers, which is good, but it also forces the stable map to remain the
large-row-byte owner. That creates four concrete limits:

1. **Mutation granularity:** appending one row or flipping one tombstone still rewrites a large page
   value through `BTreeMap<PageKey, VectorPage>`.
2. **Scan locality:** row metadata and vector bytes are interleaved in one page buffer, so the
   scoring path cannot treat vector bytes as the primary contiguous scan unit.
3. **Locator cost:** partition-page scan must map `vector_id -> subject` through
   `VECTOR_ID_TO_SUBJECT` for each candidate row, even though the row itself can carry a derived
   subject locator.
4. **Maintenance shape:** page cleanup and future tombstone compaction need a physical page
   directory and byte owner; using the stable-map value as the byte owner couples maintenance to
   map-value serialization.

These costs matter on the vector-index hot paths: `vector_search`, `vector_search_tuned`,
`admin_vector_rebuild_step` during `Building`, normal `vector_upsert`, dual-write upsert during
rebuild, and `vector_remove`.

## Existing Architecture Assessment

The current architecture has the correct domain ownership:

- `VECTOR_SUBJECT_TO_ID` is the vector-index canister's live-clock and freshness source of truth for
  derived vector rows.
- `VECTOR_INDEX_DEFS` owns vector-index configuration, including `encoding`, `dims`,
  `stride_bytes`, active `index_version`, and durable `VectorId` allocation.
- `VECTOR_PARTITION_HEADS` owns partition-level page allocation cursors and live-row counts.
- `VECTOR_PAGE` owns physical row bytes.

The problem is the physical representation of `VECTOR_PAGE`, not the canonical/derived boundary.
Moving vector rows into graph storage would make the index less derived. Moving scan policy to
Router would mix orchestration and storage. Creating a generic `ic-stable-vector-index` crate before
the shape has a second user would over-generalize the concrete problem.

Therefore the existing Vector Index domain should absorb the change by replacing the physical page
store behind a narrow vector-index-owned API.

## Alternatives

### A. Keep raw `VectorPage` values in `BTreeMap`

Keep the uncommitted raw fixed-stride page buffer and continue storing each page as a
`BTreeMap<PageKey, VectorPage>` value.

Benefits:

- Smallest code change.
- No new stable region.
- Already improves the Candid-page bottleneck.

Drawbacks:

- Still rewrites page values for append and tombstone.
- Keeps physical byte ownership inside the stable-map value layer.
- Does not remove candidate-row reverse-locator lookups.
- Leaves future tombstone cleanup coupled to page-value serialization.

This is an acceptable short-term measurement step, but not the final vector-index physical layout.

### B. Use a generic stable ordered map or deque for row storage

Store rows in `ic-stable-paged-ordered-map` or `ic-stable-vec-deque` style structures keyed by a
packed row id.

Benefits:

- Reuses existing stable crate patterns.
- Gains header validation and stable-memory layout discipline.

Drawbacks:

- Ordered map pages solve key navigation, not vector-byte scan locality.
- Deques solve sequence append, not partition-local page scan and tombstone accounting.
- Neither structure owns vector-index invariants such as fixed stride, row generation, partition
  freshness, or subject-clock validation.

Rejected because the abstraction is close mechanically but wrong semantically.

### C. Introduce a vector-index-owned slab page store

Split page ownership into a small stable-map directory and a dedicated raw stable-memory slab:

```text
VECTOR_PAGE_META[(index_id, index_version, partition_id, page_id)] -> VectorPageMeta
VECTOR_ROW_SLAB -> raw fixed-page vector row slabs
```

Benefits:

- Keeps Vector Index as the owner of vector row invariants.
- Makes append and tombstone byte-granular.
- Keeps page metadata small and range-scannable by the existing `PageKey` order.
- Allows contiguous vector-byte reads and batch scan visitors.
- Allows row-local derived subject locators while preserving `VECTOR_SUBJECT_TO_ID` as freshness
  source of truth.

Drawbacks:

- Adds at least one stable region.
- Requires layout registry and stable-memory inventory updates.
- Requires new reopen validation and partial-layout tests.
- Requires benchmark coverage for both mutation and query paths.

Accepted.

### D. Build a reusable `ic-stable-vector-slab` crate first

Create a new generic `ic-stable-*` crate for fixed-stride row slabs and use it from
`graph-vector-index`.

Benefits:

- Could be reused by future vector-like stores.
- Keeps raw stable-memory layout outside canister business logic.

Drawbacks:

- No second user exists as of this decision.
- A generic crate would either omit vector-index invariants or encode them indirectly.
- It raises API and documentation burden before the concrete store shape is proven.

Rejected for the first implementation. The store may be extracted later if another domain needs the
same invariant set.

## Decision

Replace `VECTOR_PAGE` as a large page-value store with a vector-index-owned composite slab page
store.

### Stable regions

The implementation will define the new vector-index stable layout directly. No compatibility layer
or migration path from the development `VECTOR_PAGE` representation is required or desired.

The planned shape is:

```text
VECTOR_PAGE_META[(index_id, index_version, partition_id, page_id)] -> VectorPageMeta
VECTOR_ROW_SLAB -> VectorRowSlabStore
```

`VECTOR_PAGE_META` may use the MemoryId previously assigned to `VECTOR_PAGE` in the development
layout.
`VECTOR_ROW_SLAB` is a required companion region. The two regions form a composite store and must
open together: fresh + fresh creates a new store, populated + populated reopens it, and any partial
combination fails closed.

### Page metadata

`VectorPageMeta` is small and Candid-encoded or fixed-width encoded. It carries only page-directory
facts:

```text
VectorPageMeta {
  slab_offset: u64,
  capacity: u32,
  row_count: u32,
  live_count: u32,
  row_stride: u32,
  tombstone_count: u32,
}
```

`VECTOR_PARTITION_HEADS` remains the per-partition allocator/counter owner:

- `first_page`
- `mutable_page`
- `page_count`
- `live_len`
- `next_page_id`

`VectorPageMeta` does not duplicate `index_id`, `index_version`, `partition_id`, or `page_id`; those
belong to the `PageKey`.

### Slab header

`VECTOR_ROW_SLAB` has its own magic/version header, following the `ic-stable-*` pattern:

```text
magic: "VSL"
layout_version: 1
occupied_tail: u64
layout_flags: u32
```

The slab validates magic, version, occupied tail bounds, and page-offset bounds on reopen. Invalid
layout fails closed.

### Physical page layout

Each physical page is fixed-stride within an index definition:

```text
page header:
  page_magic/version/check fields
  capacity: u32
  row_stride: u32

tables:
  vector_id       [u64; capacity]
  generation      [u64; capacity]
  subject_locator [(shard_id: u32, vertex_id: u32); capacity]
  tombstone_bits  [ceil(capacity / 8)]
  vector_bytes    [capacity * row_stride]
```

The layout is structure-of-arrays rather than row-interleaved. Search reads vector bytes as the
primary contiguous unit. Metadata reads are separated from scoring bytes, so a future SIMD or
bounded-distance scoring loop does not have to step over row headers.

`subject_locator` is a derived locator only. It is not the freshness source of truth.
`VECTOR_SUBJECT_TO_ID` remains the only live-clock and freshness authority for search hits.

### Store API

The physical layout is encapsulated by a `VectorSlabStore`-style API inside `graph-vector-index`:

```text
append_row(index_id, index_version, partition_id, def, vector_id, generation, subject, bytes)
  -> Result<SlotRef, VectorIndexError>

tombstone_row(index_id, slot) -> bool

read_row_bytes(index_id, slot, out) -> Option<RowHeader>

visit_partition_pages(index_id, index_version, partition_id, scratch, visitor)

drop_version_pages(index_id, index_version, cursor, budget) -> DropProgress
```

Callers must not read or write slab offsets directly. `append_slot`, `tombstone_slot`,
`read_slot_bytes`, partition-page search, rebuild `Building`, `Cleaning`, and `Aborting` all go
through this API.

`append_row` is fallible: the slab can fail to `grow`, so it returns `Result` and maps `GrowFailed`
to a `VectorIndexError`. Its mutation order is write-then-commit — grow + write the row tables and
vector bytes (and a fresh page header) first, then bump `occupied_tail`, then insert/update
`VECTOR_PAGE_META`, and update `VECTOR_PARTITION_HEADS` (including `live_len`) last — so a failed
grow/write can never leave a head or page-meta pointing at unwritten bytes. If a trap occurs between
the `occupied_tail` bump and the directory writes, the surplus slab bytes are acceptable leaked dead
space (see *Allocation and cleanup*).

Because `append_row` is fallible, dual-write callers (`vector_upsert` during `Building`) make both
appends — active then shadow — before any `tombstone_row`, `VECTOR_ID_TO_SLOT`, or
`VECTOR_SUBJECT_TO_ID` commit. If the shadow append fails, the caller tombstones the already-appended
active row before returning the error, so the residual is a tombstoned dead row (page-meta and
`PartitionHead.live_len` accounting restored) rather than a live-counted orphan, and the subject
clock / id map stay pointing at the prior valid slot.

`tombstone_row` owns all live/tombstone accounting idempotently: on the live→tombstoned transition it
sets the bit and adjusts `VectorPageMeta.live_count`/`tombstone_count` and the row's
`VECTOR_PARTITION_HEADS.live_len` exactly once; an already-tombstoned row is a no-op.

`visit_partition_pages` is a page/batch visitor (not a row-by-row `Vec<u8>` yield): it bulk-reads each
page's fixed-width tables (`vector_id`, `generation`, `subject_locator`, `tombstone_bits`,
`vector_bytes`) once into reusable `scratch` buffers and invokes the visitor per live slot with a
zero-copy slice into the contiguous `vector_bytes` table, preserving the structure-of-arrays scan
benefit.

### Freshness and consistency

The freshness contract from ADR 0031 is preserved:

- `VECTOR_SUBJECT_TO_ID[(index_id, subject)]` remains the source of truth for whether a subject is
  live, which `VectorId` it owns, and which slot is current for the active `index_version`.
- `SlotRef.generation` still protects stale row handles.
- Tombstoned rows are never scored.
- A row-local `subject_locator` can replace the partition-scan hot-path lookup through
  `VECTOR_ID_TO_SUBJECT`, but search still re-validates the candidate against
  `VECTOR_SUBJECT_TO_ID`.
- `VECTOR_ID_TO_SUBJECT` becomes unnecessary for the partition-scan hot path. The implementation may
  retire it during the layout cutover if no remaining API requires it.

### Allocation and cleanup

The first implementation uses tail allocation, not a general free-span allocator.

Rationale:

- Vector rows are append-and-tombstone.
- Rebuild and abort cleanup remove pages by `index_version`.
- Best-fit free-span allocation would add metadata and search cost before there is evidence it is
  needed.

Page cleanup deletes `VECTOR_PAGE_META` entries for an old or aborted version. In this first slice
there is **no slab tail rewind**: `PageKey` order is not guaranteed to match slab-tail order across
partitions, so a "removed page is the current tail" check is either unsafe or unbounded. Dropped
pages leave their slab bytes in place as dead space, and `occupied_tail` is unchanged. Reopen
validation therefore explicitly allows `occupied_tail` to exceed the highest referenced page-meta
end. General free-span reuse and slab compaction are deferred until benchmarks show stable-memory
growth or rebuild churn requires it.

## Invariants

The implementation must enforce these invariants at the Vector Index storage boundary:

1. A page's `row_stride` equals `VECTOR_INDEX_DEFS[index_id].stride_bytes` for the relevant
   `index_version`.
2. `row_count <= capacity`.
3. `live_count + tombstone_count <= row_count`.
4. `SlotRef.slot < row_count`.
5. A non-tombstoned row can be scored only after `VECTOR_SUBJECT_TO_ID` confirms that the subject is
   live, owns the row's `vector_id`, and points at the same `(index_version, partition_id, page_id,
   slot, generation)`.
6. `VECTOR_PAGE_META` and `VECTOR_ROW_SLAB` never reopen partially. The composite is keyed on raw
   slab size: `slab.size() == 0 && meta.is_empty()` is fresh; a slab with a valid header reopens
   (a **valid empty-initialized** store — empty meta with an in-bounds `occupied_tail` — is a valid
   reopen, not a partial one); any other combination (non-empty slab with absent/invalid magic,
   empty slab with non-empty meta, out-of-bounds `occupied_tail`, or a meta whose span exceeds
   `occupied_tail`) fails closed. `VECTOR_PARTITION_HEADS` is outside this composite and is not
   cross-checked on reopen, so an interrupted `Cleaning`/`Aborting` may transiently leave a head
   referencing already-deleted page-meta.
7. Search remains exact over the selected partitions. Any future early-stop budget still requires an
   explicit partial/cursor/error contract; silent truncation remains forbidden.

## Consequences

Positive effects:

- Appending a row no longer rewrites a page-sized stable-map value.
- Tombstoning a row can update one bit/byte plus small metadata.
- Partition-page scans can read vector bytes contiguously.
- The partition-scan reverse-locator cost can be removed from the hot path.
- Page cleanup has a physical directory independent from page-byte storage.
- The design matches existing `ic-stable-*` stable-memory practices without prematurely creating a
  generic crate.

Costs:

- The vector-index stable layout changes.
- At least one new stable region is required.
- Reopen and partial-layout validation must be implemented and tested.
- The codebase gains a vector-index-specific slab abstraction.
- `VECTOR_ID_TO_SUBJECT` retirement must be audited carefully if removed.

## Layout Cutover

There is no old `VECTOR_PAGE` runtime state to preserve. The implementation must not add a migration
from the development page-value representation, a compatibility reader, or a canonical
backfill/rebuild step for this layout decision.

The required cutover work is:

1. Define the new `VECTOR_INDEX_STABLE_LAYOUT`.
2. Assign a MemoryId for `VECTOR_PAGE_META`.
3. Assign a required companion MemoryId for `VECTOR_ROW_SLAB`.
4. Initialize both regions as the only supported vector page-store format.
5. Reject partial page-meta/slab reopen state for the new composite store.

## Test Requirements

Required tests:

- Fresh init creates both page-meta and slab regions.
- Reopen rejects partial page-meta/slab layouts.
- Append writes row metadata and vector bytes at the expected slot and returns a valid `SlotRef`.
- Tombstone is idempotent and updates page/partition live counts once.
- `read_row_bytes` rejects tombstoned rows, stale generations, and out-of-range slots.
- Partition scan uses row-local subject locators but still rejects stale candidates via
  `VECTOR_SUBJECT_TO_ID`.
- Rebuild `Building` writes shadow rows through the slab store.
- `Cleaning` and `Aborting` delete page metadata for the right `index_version`.
- `nprobe = nlist` still matches the exact subject-map scan.

## Benchmark Requirements

The full `canbench --persist` suite was run (no pattern filter) and persisted to
`crates/graph-vector-index/canbench_results.yml`. Instruction counts improved on every affected path
versus the prior Candid page layout — search, normal and dual-write `vector_upsert`, the rebuild
`Building` step, and full rebuilds — with the new `bench_remove_normal_d128` `vector_remove`
baseline added. Coverage:

- normal `vector_upsert` (`bench_upsert_normal_d128`)
- rebuild dual-write `vector_upsert` (`bench_upsert_dualwrite_d128_nlist16`)
- `vector_remove` (`bench_remove_normal_d128`, new)
- rebuild `Building` step (`bench_rebuild_building_step_d128_nlist16`) and full rebuilds
- exact subject-map scan (`bench_vector_search_*`)
- partition-page scan for representative `dims`, `nlist`, and `nprobe` (`bench_ivf_*`)

## Design Documentation Impact

Implementation must update:

- [index/vector-index.md](../index/vector-index.md) — physical page/slab layout, freshness contract,
  and `VECTOR_ID_TO_SUBJECT` status.
- [storage/stable-memory-inventory.md](../storage/stable-memory-inventory.md) — vector-index region
  count and MemoryId assignments.
- [adr/README.md](README.md) — ADR index.
- [README.md](../README.md) — design document map if this ADR is listed there.

ADR 0031 remains the parent ownership and consistency decision. This ADR narrows only the derived
vector-index physical page store.

## Alternatives Considered

See [Alternatives](#alternatives). The accepted path is Alternative C: a vector-index-owned slab page
store.
