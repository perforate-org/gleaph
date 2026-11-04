# 0050. LARA labeled traverse read API consolidation

Date: 2026-07-25
Status: implemented
Last revised: 2026-09-29
Implementation status: consolidated `labeled::graph::traverse` is implemented. Selected slab/spill reads coalesce adjacent logical slots; hybrid value batches use range reads. `HybridOverflowEdgeReplay`, the trait replay capability, and directional replay/scratch adapters are removed. Property-first phase two uses canonical `visit_edges_at` reads; property bytes remain in the phase-one scratch buffer.

Implementation history (completed consolidation; method names below describe those phases): Phase 1/2 substrate (`labeled::graph::traverse_next`) and Phase 2 benchmarks are in place; CounterpartScan consumes `read_edge_state` and `visit_edges`; the physical-location reader `visit_storage_edge_locations` is implemented and the legacy `read_physical_edge_at_slot_for_label` / `for_each_live_physical_edge_location_for_label` methods are deleted; selected-slot APIs (`read_out_edge_slots_for_label*` / `read_in_edge_slots_for_label*`) now accept typed `BucketEntryPosition` instead of raw `u32`; the new module has local implementations for state reads and selected-slot traversal, so the legacy internal helpers `for_each_edges_for_label_topology_unchecked`, `for_each_live_edge_slot_for_label_desc`, `read_edge_slot_state_for_label`, and `for_each_live_edge_slot_for_label` have been removed; adoption-fixture and mate-adoption-gate physical scans are migrated to the new surface. Phase 4 completed: `for_each_edges_for_label_ordered` retains its measured dense, tombstone-free bulk-read fast path in the legacy wrapper and delegates hybrid/sparse buckets to `traverse_next::visit_edges_with_inline_property`; `for_each_edges_for_label_topology_ordered` and `for_each_edges_for_label_unchecked` delegate to `traverse_next::visit_edges` / `visit_edges_with_inline_property_unchecked`; the dead `skip_then_visit_each_out_edge_for_label` and `skip_then_visit_each_out_edge_by_directedness` legacy methods are removed; `visit_edges_with_inline_property_unchecked` was added to `traverse_next`; `DeferredBidirectionalLabeledGraph` unchecked label wrappers route through the unchecked visitor. Phase 4b completed: the `read_out_edge_slots_for_label*` family and its internal helpers are removed; tests now call `traverse_next::visit_edges_at_with_replay`. Phase 4c completed: `visit_label_out_edges_inner` now delegates to `traverse_next::visit_all_labels_with_inline_property`; new `visit_all_labels` and `visit_all_labels_with_inline_property` visitors cover all label buckets with the same dense/hybrid/sparse per-bucket fast paths; public `visit_out_edges` / `visit_asc_out_edges` / `out_edges` / `asc_out_edges` now route through the consolidated visitor. Phase 4d completed: added `visit_out_edges_by_directedness` (topology-only) to `traverse_next`; `iter_out_edges_by_directedness` / `iter_out_edges_directed_only` / `iter_out_edges_undirected_only` collect via the new visitor. Phase 4e completed (2026-07-28 UTC): `LabeledOutEdgesIter` is now bucket-lazy and delegates each bucket to `traverse_next::visit_edges_with_inline_property`; `labeled_out_edges_iter`, `desc_out_edges_iter`, `asc_out_edges_iter`, and `out_edges_by_directedness_iter` no longer use legacy slab/overflow-log streaming internals; the unused `out_edges_iter_for_label*` family is removed. Phase 4f completed (2026-07-27 21:06:54 UTC): `labeled_bucket_span_iter` internals moved into `traverse_next::single_bucket_span_iter`; `visit_edges`, `visit_edges_window`, and `visit_edges_with_inline_property_impl` now use it for sparse/hybrid buckets; the dense bulk-read fast paths remain in `traverse_next`. Phase 4g completed (2026-07-27 21:28:53 UTC): the legacy public batch/value entry points `visit_out_edge_inline_property_batches_for_label` and `visit_out_inline_property_batches_for_label` now delegate to their `traverse_next` counterparts; all private dense/hybrid/sparse batch helpers that duplicated the `*_next` implementation were removed from `traverse.rs`. Phase 5 completed (2026-07-27 21:46:51 UTC): `traverse_next` was renamed to `traverse`, the legacy `traverse.rs` module was deleted, and the consolidated module now hosts both the canonical read surface and the `ContiguousBucketRun` maintenance helper previously owned by the legacy module. The public API signatures and caller imports are preserved; internal callers were updated from `graph::traverse_next` to `graph::traverse`. Phase 6 completed (2026-10-29 UTC): sparse/hybrid bucket scans moved from `single_bucket_span_iter` (decoded replay plus per-edge iterator dispatch) to the `visit_slab_bucket_span_edges` visitor; `LabeledSpanIter`, `LabeledBucketScan`, `LabeledBucketScanKind`, `single_bucket_span_iter` and `bucket_span_iter` are deleted.
Adoption status: partially activated (CounterpartScan + first Graph scan-only caller + physical-location adoption fixtures + edge-property sidecar group via Plan 0186)

## Context

Before consolidation, `crates/ic-stable-lara/src/labeled/graph/traverse.rs` had an ad-hoc collection
of labeled bucket read APIs. That surface mixed:

- bucket-local logical slots with raw slab/overflow-log locations;
- live topology reads with inline-property attachment;
- live-only reads with tombstone-aware state reads;
- ascending and descending order encoded either in names or arguments;
- point, selected-slot, visitor, iterator, and materialized result shapes; and
- general traversal with specialized dense/hybrid/sparse inline-property pipelines.

Examples include:

- `for_each_live_edge_slot_for_label` and `for_each_live_edge_slot_for_label_desc`;
- `out_edges_iter_for_label` and `out_edges_iter_for_label_ordered`;
- `read_out_edge_slot_for_label` and `read_edge_slot_state_for_label`;
- `read_out_edge_slots_for_label` and its replay-aware variant;
- `read_physical_edge_at_slot_for_label` and the adoption-fixture physical-location iterator; and
- the `visit_*_inline_property_batches_for_*` family.

The duplication makes it difficult to identify the cheapest correct primitive. More importantly,
several APIs use `u32` for two different slot domains, and the word `topology` currently means
"live edge row without inline-property reads" in some call sites rather than "raw stored cell".

The property-model terminology is also changing. The target term is **inline property**. Existing
implementation identifiers now use `inline_property` and `inline_property_bytes`; historical
`inline_property` and `payload` names in this area have been superseded by ADR 0051. New public or
crate-public read APIs defined by this ADR use `inline_property`; internal adapters call renamed
inline-property storage helpers.

## Problem

The read boundary lacks one canonical vocabulary for:

1. the slot identity used by `EdgeHandle` and CounterpartScan;
2. raw physical slab/overflow-log locations used by storage maintenance and selected internal
   fast paths, not by the canonical logical read API;
3. a live edge row with or without its inline property;
4. a missing, tombstoned, or live logical slot; and
5. optimized selected-slot and batched inline-property reads.

A naming-only rewrite is insufficient. It would either preserve the ambiguous `u32` slot contract or
remove specialized reads required by Graph predicate execution and replace them with full-bucket
materialization.

## Existing architecture assessment

Labeled LARA already owns the required canonical state and execution flow:

- the labeled bucket owns its bucket-local live ordering and logical slots;
- the edge slab and overflow log own raw storage locations;
- the inline-property slab/log owns property bytes and their live-ordinal association;
- `LabeledLaraGraph` can resolve one orientation without exposing slab/log encoding; and
- the bidirectional wrapper owns forward/reverse selection and CounterpartScan.

No new storage subsystem, persistent index, or public Graph-owned mapping is required. The
consolidation is developed first in a temporary sibling module and only becomes the `traverse`
module after caller migration and validation complete.

The specialized selected-slot and batched readers have distinct contracts. They preserve contiguous
range reads, inline-property-first predicate execution, and reusable scratch allocation. These
capabilities remain LARA-owned and must not be expressed as a generic full-bucket `Vec`.
Hybrid replay is no longer needed: descriptor/run state directly addresses selected ranges.

## Decision

### 1. Distinguish logical identity from raw physical location

The canonical labeled read surface uses a typed bucket-local logical slot:

```rust
#[repr(transparent)]
pub struct BucketEntryPosition(u32);
```

`BucketEntryPosition` is:

- the slot returned by canonical labeled scans;
- the slot stored in `EdgeHandle`;
- the slot accepted by CounterpartScan; and
- the position within the bucket's logical extent, including tombstoned positions, that LARA
  remaps across slab and overflow-log storage;
- not a compacted live ordinal: tombstones remain addressable at their logical position and do not
  get removed from this address space; and
- distinct from ADR 0048's `PairOrdinal`, which is computed only over the currently live
  equal-target subsequence.

`BucketEntryPosition` is a **query-time location**, not a stable logical edge identifier. It is valid
only within the read or mutation boundary that produced it. The current federation contract has
the same rule: `GlobalEdgeId` and encoded edge IDs are query-time handles and are not stable across
compaction. No generation or row fingerprint is added to this API because that would change the
existing wire and posting identity shape rather than strengthen the owning invariant.

Callers must not persist a slot as an independent edge identity or reuse an `EdgeHandle` after an
operation that can remove, compact, or renumber that row. A stale caller-supplied handle is outside
the read contract; this ADR does not promise generation-based stale-handle detection. The
persisted sidecar/index keys are a separate maintenance concern and remain correct through the
move observer below.

Every slot-renumbering mutation is part of the existing `EdgeSlotMove` contract. LARA emits the
move to the bidirectional owner, and the owner must deliver it to all registered consumers before
the maintenance operation is reported complete. The Graph-side move observer is the single repair
path for:

- canonical `EDGE_PROPERTIES` keys;
- edge-property posting keys and pending re-key operations; and
- legacy `EDGE_ALIASES` keys and canonical targets until ADR 0048 phase 5 removes aliases.

No read API may retain a `BucketEntryPosition` across that move boundary. If a caller needs the edge
after compaction, it must consume the moved handle or re-resolve it from canonical adjacency. The
move path, rather than a generation catalog or second slot-to-edge catalog, remains the source of
truth.

### 1.1 One logical-slot type and separate storage-key boundaries

`BucketEntryPosition` is the single canonical row-local logical-slot type. It is defined in the
crate-level `traverse` module and is reused by labeled traversal, `CanonicalEdgeOccurrence`,
Graph `EdgeHandle`, and Graph-kernel `EdgeSlotIndex`. The latter is only a domain alias for the
same type; it is not a second slot representation.

Raw `u32` conversion is confined to wire and stable-key boundaries (`raw`, `from_raw`, and the
explicit key codecs). LARA physical slab offsets, overflow-log entry indices, and inline-property-bytes
ordinals remain separate types/domains and must not be converted through this logical-slot type.

The label MSB encodes directedness. The reverse-in alias marker (`0x8000_0000`) is unrelated: it
is a transitional Graph alias-key orientation encoding used to distinguish a reverse/incoming row
from a forward/outgoing row carrying the same directed label. It is never part of
`BucketEntryPosition`, `CanonicalEdgeOccurrence`, or `EdgeHandle`; new traversal/counterpart code
uses the explicit orientation field. Alias removal remains governed by ADR 0048. The current Graph alias codec validates that a logical slot has bit 31 clear before encoding; only the codec may set that bit for the transitional reverse-in marker.

ADR 0048's phrase "canonical edge occurrence" means the identity of one persisted adjacency occurrence
at the LARA boundary. It does **not** mean a raw slab offset or overflow-log entry index.

The existing `CanonicalEdgeOccurrence` name belongs to ADR 0048 and remains the logical counterpart
occurrence `(orientation, owner, label, logical slot)`. This ADR does not redefine it. Raw storage
geometry uses separate diagnostic/storage types:

Mutation APIs must not accept a caller-supplied slot-only handle as authority for a sidecar update.
Before property, index, alias, or deletion mutation, LARA re-resolves the edge from canonical
adjacency and validates the expected owner, label, orientation, and target context. Only that
current resolved handle may be used for sidecar access. A slot reused after deletion is therefore
not treated as the old edge; a mismatch is a typed not-found/identity error. Generation or row
fingerprints are unnecessary because stale slot-only handles are not a valid mutation input.

```rust
pub(crate) enum StorageEdgeLocation {
    Slab {
        absolute_slot: u64,
        bucket_local_slot: u32,
    },
    OverflowLog {
        leaf: u32,
        entry_index: u32,
    },
}

pub(crate) struct StorageEdgeRef {
    pub(crate) owner: VertexId,
    pub(crate) label: BucketLabelKey,
    pub(crate) location: StorageEdgeLocation,
}
```

`StorageEdgeLocation` is not a substitute for `BucketEntryPosition` or ADR 0048's `CanonicalEdgeOccurrence`.
The location enum alone is not globally meaningful. Its fields are constructed by LARA, not by
callers. LARA validates that the owner/label, slab absolute/local pair, or overflow-log leaf/entry
belongs to the selected bucket before returning it. A mismatched owner, label, leaf, or bucket
must fail closed. The current high-bit overflow-log encoding is an internal implementation detail
and is not part of the canonical logical read API.

### 2. Preserve the existing meaning of topology

Within the traversal API, **topology read** means a live edge row read without attaching inline
property bytes. It does not mean "return raw tombstone cells".

Tombstone-aware point reads return an explicit state:

```rust
pub enum EdgeSlotState<E> {
    Missing,
    Tombstone,
    Live(E),
}
```

The tombstone encoding inside `E` remains private to LARA. Callers never receive an `E` that they
must inspect to discover whether it is a tombstone.

No tombstone-visible full-bucket scan is added until a concrete production caller requires one.
Storage validation and repair may use a private raw-cell iterator. If a public tombstone-visible
scan is later justified, it must yield an explicit state type rather than a tombstone-shaped `E`.

### 3. Separate inline-property attachment in method names

Attaching inline property bytes performs additional storage reads. The canonical API therefore
uses separate methods and a return type that makes the bytes observable without relying on an
optional `CsrEdge` hook. For callback-only consumers, the traversal surface exposes a borrowed view:

```rust
pub struct InlinePropertyBytesRef<'a> {
    pub width: u16,
    pub bytes: &'a [u8],
}

pub struct EdgeWithInlinePropertyRef<'a, E> {
    pub edge: E,
    pub inline_property: InlinePropertyBytesRef<'a>,
}
```

The borrowed view is valid only during the callback. Dense buckets borrow directly from the
bulk-read property span, while sparse and hybrid paths borrow from reusable property range
buffers. None of these paths constructs an owned inline-property value for the
borrowed API. Callers that retain an edge result (for example an execution binding) must
explicitly materialize owned bytes inside the callback, only when retention is required.

LARA constructs and validates the borrowed view, enforcing `bytes.len() == width`. Width zero is a
valid value and is represented by an empty slice. `CsrEdge::with_stored_inline_property_bytes` may
be used by a consumer as an explicit materialization helper, but it is not the contract and its
default no-op implementation must never be able to hide a missing property read.

For a non-zero width, the bytes must be read from the exact inline-property ordinal belonging to
the live row identified by the requested logical slot. This identity check applies independently
to slab and overflow-log storage and to forward and reverse buckets. A missing row, short or
overlong inline property bytes, malformed inline property bytes, width mismatch, or bytes belonging to another ordinal is a
`LabeledOperationError`; implementations must not zero-fill, borrow an adjacent row, or downgrade
the condition to `Missing`. Width zero performs no property read and returns an empty slice.

- names without a suffix return live topology only; and
- the canonical `Traversal::visit_edges_with_inline_property` callback receives the borrowed
  `EdgeWithInlinePropertyRef<'a, E>` view for each live row.

There is no owned inline-property traversal result in the LARA surface. A callback consumer must
not pay for a property copy unless it explicitly clones the borrowed bytes or materializes them
into its own edge value.

Boolean `attach_inline_property_bytes` flags are private implementation details and are
not exposed in the consolidated surface.

### 3a. Concrete edge value split

The Graph Kernel concrete edge value follows the same boundary without a compatibility shape:

```rust
pub struct Edge {
    pub target: VertexRef,
    pub edge_slot_index: EdgeSlotIndex,
    pub label_id: u16,
}

```

`Edge` is topology-only. It must not contain an optional, empty, or compatibility inline-property
field. `EdgeWithInlinePropertyRef<'a, E>` is the only traversal value shape that exposes attached
inline property bytes. For the Graph Kernel adapter, `E` is the topology-only `Edge`.

The borrowed view owns no storage and is valid only for the callback invocation. The property-read
owner, LARA, validates the exact width and ordinal before invoking the callback. This is an
intentional breaking refactor: consumers that need to retain the bytes explicitly clone them or
materialize them into a separately owned application value. The
`CsrEdge::with_stored_inline_property_bytes` hook remains only as an internal materialization
helper, not as the public read contract.

The target term `inline_property` is used consistently. `payload` is not used in new API names
because it is broader and can be confused with other edge inline property bytes or property-store
concepts.

### 4. Order is an argument

Ascending and descending traversal use `OutEdgeOrder`. Direction is not encoded in method names.
(Amended by Plan 0195, ascending-default: unsuffixed traversal methods are ascending by default, and
only the explicit descending opt-in encodes direction in its name (`desc_out_edges_iter` /
`OutEdgeOrder::Descending`); the former `asc_*` aliases were removed.)

The order applies to `BucketEntryPosition`, including edges whose bytes currently reside in an overflow
log. For a Graph Type `ORDER BY INSERTION` label, ascending/descending traversal is the semantic
insertion order. For an unordered label, `OutEdgeOrder` only describes the chosen physical traversal
direction; it must not be exposed as a query-level insertion-order guarantee. Raw physical-location
enumeration has its own storage-defined order and is not covered by `OutEdgeOrder`.

### 5. Visitors use explicit control flow

Streaming methods are named `visit_*`, not `*_iter`. They use `ControlFlow` instead of a boolean
whose `true`/`false` meaning can be inverted by callers:

```rust
use std::ops::ControlFlow;
```

A visitor returns `ControlFlow<B>`:

- `ControlFlow::Continue(())` continues; and
- `ControlFlow::Break(value)` terminates early and returns the caller's value.

LARA storage errors remain the outer `Result`. Callers that need a domain error can use the break
value to carry it without merging storage and caller error types.

### 6. Canonical general-purpose read surface

The crate-level `Traversal` trait is the shared contract for every LARA implementation. Its associated
`Request`, `Slot`, `EdgeState`, `EdgeWithInlinePropertyRef<'a>`, and `Error` types allow
normal, labeled, bidirectional, and deferred backends to expose the same logical operations. `Edge`
is bounded by the existing `CsrEdge` trait; storage codec and inline property bytes behavior
therefore remain single-sourced. The trait includes topology point reads, state reads, borrowed
inline-property visits, full visits, and selected-slot visits. No associated replay state is exposed.
Owned property materialization is a concrete/facade concern rather than part of this core trait.
The isolated labeled implementation is the first adapter; other backends are added against this
contract.

The smallest general-purpose surface is:

```rust
pub(crate) fn read_edge(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    slot: BucketEntryPosition,
) -> Result<Option<E>, LabeledOperationError>;

pub(crate) fn read_edge_state(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    slot: BucketEntryPosition,
) -> Result<EdgeSlotState<E>, LabeledOperationError>;

pub(crate) fn visit_edges<B>(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    order: OutEdgeOrder,
    visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, LabeledOperationError>;

pub(crate) fn visit_edges_with_inline_property<B>(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    order: OutEdgeOrder,
    visit: impl for<'a> FnMut(
        BucketEntryPosition,
        EdgeWithInlinePropertyRef<'a, E>,
    ) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, LabeledOperationError>;

pub(crate) fn visit_edges_at_with_inline_property<B>(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    order: OutEdgeOrder,
    slots: &[BucketEntryPosition],
    visit: impl for<'a> FnMut(
        BucketEntryPosition,
        EdgeWithInlinePropertyRef<'a, E>,
    ) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, LabeledOperationError>;
```

The execution surface also exposes a reusable window without putting paging state into the
request object:

```rust
pub struct TraversalWindow {
    pub offset: u32,
    pub limit: Option<u32>,
}

fn visit_edges_window<B>(
    &self,
    request: &Self::Request,
    window: TraversalWindow,
    visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, Self::Error>;

fn visit_edges_with_inline_property_window<B>(
    &self,
    request: &Self::Request,
    window: TraversalWindow,
    visit: impl for<'a> FnMut(
        Self::Slot,
        Self::EdgeWithInlinePropertyRef<'a>,
    ) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, Self::Error>;
```

`TraversalRequest` remains the identity of the logical edge set (owner, label, direction, and
order). `TraversalWindow` is execution-time control; `None` is unbounded. Reaching the limit is normal
`ControlFlow::Continue(())`, while a visitor `Break` propagates unchanged. A zero limit invokes no
visitor. The default adapter, including the inline-property window adapter, counts matching live
rows. The labeled **topology** override `visit_edges_window` instead cuts tombstone-inclusive
positions in request order, as specified by ADR 0088: ascending position is the slot itself;
descending position is `extent - 1 - slot`. Holes consume positions but yield no callback. This
existing topology contract permits a direct range read even when the row contains tombstones.
Selected-slot APIs keep their own explicit slot set and are not implicitly windowed, preventing
paging state from being applied twice by property-first Graph execution.

GQL `OFFSET`/`LIMIT` may use this capability only when the planner proves that traversal order is
the result order and no later sort, distinct, aggregation, or other row-expanding operation changes
cardinality. Otherwise Graph continues to apply the window at the row-execution boundary.

#### Known gap: tombstone-aware offset acceleration

**Resolved for tree buckets (2026-09-07):** [ADR 0094](0094-ltb-block-tombstone-count-level1-offset.md)
adds a per-block `u16 tombstone_count` to the LTB block header (single-funnel maintenance,
fail-closed scan self-verification) and restructures `visit_edges_window`'s tree arm to resolve
only the blocks overlapping the window's position range, skipping fully-dead in-window blocks
(Plan 0338; measured attribution in the ADR implementation-status appendix). The tree window cost
is bounded by the window extent, not the bucket extent. Slab and bypass topology windows now also
read only their selected logical range through the contiguous row reader below. This does not add
live-rank acceleration to the default inline-property window adapter. Research investigation §5.5's
slab overshoot (~109K instructions/query at the post-ADR-0088 `T_PROMOTE` extent cap) describes the
prior execution path, not the cost of this direct slot-range reader. Remove-side maintenance
admission remains a separate decision.

The current contract does not introduce persistent tombstone-count, live-count, bitmap, or
rank/select metadata **for slab and bypass buckets**. A live-row-counting property window may still
inspect a sparse prefix to count live matches. That limitation does not apply to the labeled
topology window's tombstone-inclusive slot offset.

Persistent interval summaries and live-bitmaps are a future research/implementation topic. Candidate
designs include fixed-width logical-slot block summaries, hierarchical live-count directories, and
bitmap rank/select structures. Any future implementation must first establish its update,
compaction/rebuild, stable-memory, overflow, forward/reverse, corruption, and benchmark contracts.
It must remain derived metadata owned by LARA and must never change `BucketEntryPosition` identity or
allow an unverified summary to skip a live edge. Until that work has an accepted design and measured
benefit, exact live-rank scanning remains authoritative for property windows.

#### Contiguous slab/spill topology execution

**Implemented:** `EdgeStore::visit_row_edges` owns direct topology traversal for slab-mode and
bypass rows. `initialized_row_slots` is the shared checked prefix/run extent decision;
`initialized_slab_slots` projects its prefix for existing callers. The row's `CsrVertex` contract
decides whether its stored width is prefix-only or includes the spill. Used length remains in the
run header, never in a duplicated descriptor or read-side cache.

Full scans and topology windows read each intersecting plane once: the initialized slab prefix
through the slab reader, and the used spill suffix through `SpillRunStore::read_rows`. Range and
allocation arithmetic is checked; run validation still rejects slack and corrupt lengths. The
reader decodes in the requested order, skips tombstones without renumbering slots, and propagates
`Break`. It neither builds/reverses replay entries nor reads successor descriptors to infer
initialized width. Labeled traversal attaches label identity after tiny/tree mode dispatch.

Topology visitors, zero-width borrowed/materialized property visitors, and the topology portion of
existing dense property visitors share this primitive. `visit_row_edge_slots` shares its physical
range reader and decoder, but reads only adjacent selected runs. Full scans buffer their initialized
extent, as the dense slab reader already did; topology windows buffer only the selected range.
`Break` stops further decoding and callbacks, but does not cancel an already-completed bulk read;
selected reads do not read subsequent runs after a break. Core streaming iterators and tree readers
keep their separate execution paths. No fold, capacity, promotion, layout or write policy changes
are part of this read optimization.

[Focused measurements and contract tests](../evidence/adr0097-contiguous-spill-traversal.md).

Ordinary materialization collects `visit_edges` output explicitly. The bounded complete
collectors below are the exception: they own allocation so an incomplete scan cannot publish a
successful prefix or invoke an external callback before exhaustion has been established.

All logical readers share the same corruption boundary. A missing label, an out-of-range logical
slot, or a tombstone is a normal absence/state result according to the method contract. In
contrast, a malformed overflow chain, bucket-owner or label mismatch, an impossible
logical-slot-to-physical mapping, or an inline-property ordinal mismatch is
`LabeledOperationError` for `read_edge`, `read_edge_state`, `visit_edges`,
`visit_edges_at_with_inline_property`, and `visit_edges_with_inline_property`. These conditions must never be represented
as `None`, `Missing`, an empty visit, or fabricated property bytes.

There is no point-read inline-property API: inline property bytes exist only for a live row, and
the borrowed selected-slot visitor is the zero-copy operation for callers that need both the edge
and its property. A caller that needs ownership clones within the callback. A measured point-read
caller may build that operation on top of `visit_edges_at_with_inline_property` later.

The bidirectional wrapper does not define a second read primitive. Its forward/outgoing methods
delegate to the forward LARA graph, and its reverse/incoming methods delegate to the reverse LARA
graph with the same logical-slot and `OutEdgeOrder` contract:

```rust
for_each_out_edges_for_label*  -> forward.visit_edges*
for_each_in_edges_for_label*   -> reverse.visit_edges*
read_out_edge_slots_for_label* -> forward.visit_edges_at
read_in_edge_slots_for_label*  -> reverse.visit_edges_at
```

The `in`/reverse wrapper uses the destination vertex as the owner of the reverse bucket. It must
preserve the same missing, tombstone, ordering, early-break, and inline-property return contracts;
only the orientation and error wrapper differ.

### Bounded complete topology collection (implemented)

Verified: 2026-09-13 00:22:26 UTC +0000.

`LabeledLaraGraph::collect_edge_topology_bounded(owner, label, max_slots)` returns all live
`(BucketEntryPosition, E)` pairs in ascending logical-slot order, or an error without a partial
result. Parallel edges remain separate; tombstones consume positions. The forward wrapper
`collect_forward_edge_topology_bounded` only delegates and wraps errors. This is a LARA read
capability, not a Graph mutation, Router admission rule, or bulk edge SET endpoint.

For a nonempty label, the limit counts **physical slab/tree slots plus linked overflow entries**,
including tombstones. The authoritative live count and stored extent are checked before result
allocation or slab/tree payload reads. The log reader checks its remaining allowance before every
entry read, requires the `-1` terminator, and rejects invalid pointers/cycles. An observed live-count
mismatch is an error, not truncation. Missing labels and authoritative zero-live-count labels return
an empty vector without scanning tombstones. Existing vertex-range errors remain errors; endpoint
liveness/visibility and policies belong to Graph, not this topology reader.

The collector uses descriptor fields directly, the existing constant-read bypass slab-geometry
owner, and the physical-depth tree walker. It does not count a chain with the unbounded metadata
API, construct a selected-slot replay, prefetch an entire leaf log table, or attach inline properties.
All candidate data remains private until the complete read succeeds. No canonical write, physical
remapping, await, or persistent cache is performed; ordinary visitor paths are unchanged.

Resource accounting for caller limit `W`, slab slots `S`, and edge type `E`:

- Result capacity is at most `W` pairs. Slab bytes are at most `S * E::BYTES`; head-first overflow
  capacity is at most `min(W-S, allocated_log_entries)` edges. No vector grows past its reservation.
  Consequently variable reader-owned workspace is bounded by
  `W * (size_of::<(BucketEntryPosition,E)>() + max(E::BYTES,size_of::<E>()))`, plus bounded descriptor
  lookup (at most `MAX_BUCKETS_PER_VERTEX`, currently 1024), fixed scratch, and the 4 KiB tree buffer.
  Allocator overhead and allocations inside a custom edge codec are excluded.
- Slab/log topology payload reads are at most `W * (E::BYTES + 4)` bytes. Four-byte trees read at
  most `ceil(W/1024) * (4096 + 4 * physical_depth)` payload bytes; depth comes from the existing
  descriptor, not structural depth inferred from degree. These formulas exclude fixed-size
  per-access store headers/address translation and descriptor lookup. They are not exact total
  `stable_read` or instruction counts.
- This API has one physical-slot limit, **not an independent byte-budget parameter**. Graph must
  choose its slot allowance using its concrete edge representation and workspace budget, then
  separately bound property/policy reads, mutations, derived operations and response bytes.

Six native contracts cover exact/one-short limits, parallel neighbors, source/label absence,
invalid owners, slab and flag-encoded tombstones, hybrid property separation, live-count mismatch,
physical depth 1/2 trees, unreadable roots beyond rejection, log termination and invalid links.
A counting-memory test reads only 12 bytes (allocation cursor + link + four-byte edge) for a
one-entry chain in a leaf with 64 allocated entries; the ordinary prefetch control reads the table.
This is a native storage-owner observation, not a whole-Graph byte measurement. Bypass slab reads
are exercised; overflow-backed bypass dispatch shares the existing geometry owner but has no new
end-to-end lifecycle fixture in this slice.

Focused, unpersisted `canbench bounded_topology` measured 36.64K instructions for 64 edges,
405.58K for a complete 4096-slot slab, 260.22K for a complete 4097-slot tree, and 2.94K to reject a
65,537-slot tree with `W=4096`. Setup and correctness assertions are outside measurement; result
allocation and destruction are inside. All closures showed zero additional heap/stable pages,
not zero temporary memory. `canbench tree_visit_stop` retained 11.83K first-stop costs and
268.10K/4.11M full walks. These are topology measurements, not bulk SET throughput.

The API adds no persistent bytes, layout, journal, edge identity, or migration/reinstall requirement.
Returned positions are valid only within a mutation-free local segment, not across writes or awaits.
Bounded sidecar properties, policy eligibility, whole-row preflight, canonical SET/outbox/outcome
atomicity, and durable rejection/replay remain unimplemented bulk-edge requirements. The following
inline-property collector extends this topology prerequisite without implementing those mutations.

### Bounded complete inline-property collection (implemented)

Verified: 2026-09-13 01:26:43 UTC +0000.

`collect_edges_with_inline_property_bounded(owner, label, max_slots, max_value_bytes)` returns all
ascending `(BucketEntryPosition, E, InlinePropertyBytes)` rows or an error without a partial result.
`collect_forward_edges_with_inline_property_bounded` only delegates and wraps errors. The existing
`InlinePropertyBytes` owns the width/byte-length invariant and small-value storage; generic `E` is
not required to implement property attachment. Parallel neighbors and tombstone-reserved topology
positions remain distinct. Missing/empty labels return no rows; bypass and width-zero labels return
empty property values. Positions remain ephemeral within one mutation-free, no-await local segment.

Execution and admission:

1. Complete the bounded topology read above. Its slot limit and geometry/resource contract apply.
   No property value is read by this phase; byte rejection may still pay the topology cost.
2. For non-tree, positive-width buckets require `property_slab_slots + property_log_len == live_count`.
   Slab/log properties follow **live ordinals**, not tombstone-inclusive edge slots. Tree properties
   instead use physical slots, preserving holes in the independent tree stream.
3. Before any property-value read or inline-result/value-buffer allocation, check the complete **body-byte**
   requirement. For live rows `L` and width `w`, this is `L*w`, plus `M-w` if wide property-log blobs
   exist, where `M=65535` is owned by the blob writer's existing u16-length format. Arithmetic or
   allowance overflow returns `LaraOperationError::ReadByteLimitExceeded`.
4. Wide blob retrieval decodes the whole body before checking it against the bucket width. The
   reserve covers the worst final mismatched blob after preceding valid bodies; a mismatch errors
   immediately. It is conservative: even a valid 32-byte log body needs this headroom. The bound
   assumes blob encodings produced by the owning writer, not arbitrary corruption of B-tree lengths.
5. Reconstruct exactly the descriptor's u8 property-log count, checking allocated indices and the
   final `-1` terminator. The private reader never follows an extra link or prefetches the leaf table.
   Count/pointer/cycle inconsistencies return `LogChainShort`, never a truncated successful chain.
   Bulk-read the admitted slab prefix once, reuse one value buffer for tree/log reads, and assemble
   private results with the existing small-value container. Ordinary visitors are not rerouted.

`max_value_bytes` is **not total stable I/O, a heap ceiling, or an instruction meter**. It bounds
selected value bodies, excluding fixed-width log cells/links, blob length prefixes, tree pointers,
B-tree search metadata, address translation, allocator overhead and custom edge-codec allocations.
Property-log length is at most `min(W,255)` for physical slot allowance `W`; each chain entry reads
one four-byte link/eight-byte cell and each selected log value reads its fixed entry again. Tree
property reads request exactly `w` payload bytes per live slot plus depth-bounded pointer resolution.
B-tree search overhead must be considered separately from its admitted body size.

A conservative reader-workspace bound adds the following to the topology bound above:
`W * size_of::<(BucketEntryPosition,E,InlinePropertyBytes)>() + 2*B + w + 4*min(W,255)`, where
`B=max_value_bytes`, plus at most `2*M+2` bytes of blob decoding/copy scratch when wide log blobs
exist. The `2*B` allowance covers the slab scratch and owned result bodies; small values reside in
`InlinePropertyBytes` itself. Fixed descriptor/search scratch and allocator/codec exclusions still
apply. This deliberately overestimates buffers whose lifetimes do not overlap. Graph must choose
numerical slot/value/workspace limits rather than treat a returned-row cap as a complete work cap.

Six new native contracts cover zero/exact/one-short bounds, parallel values, live-ordinal slab/log
mapping, missing/zero-width labels, property count inconsistencies, wide-blob headroom and a maximum
width-mismatched blob, bounded chain termination, and tree property holes/unreadable roots. The tree
fixture explicitly invokes the existing promotion primitive and seeds an encoded tombstone;
property-bearing tree insertion/removal dispatcher support is **not** established by this fixture.
The internal property resolver also rejects out-of-range slots with `EdgeSlotOutOfRange`, replacing
its block-zero sentinel; see [ADR 0088](0088-tree-csr-mode-for-high-degree-label-buckets.md#tree-property-slot-bound-implemented).

Focused, unpersisted `canbench bounded_inline` measured 45.33K instructions for 64 eight-byte values,
2.29M for 4096 values, and 474.00K for a one-byte-short 4096-value rejection. The same source fixture
initially cost 4.28M with per-value slab reads/allocations, then 3.86M with a slab bulk read; reusing
`InlinePropertyBytes` small-value storage reduced it to 2.29M (about 46% below the initial attempt).
The return container changed during this optimization; logical rows/bytes and measured allocation/
destruction coverage did not. This is not a comparison against an existing deployed API. All three
closures showed zero additional heap/stable pages, not zero memory. Topology controls retained
36.64K/405.58K/260.22K/2.94K instruction results. These are read costs, not SET/policy throughput.

No persistent layout, MemoryId, blob encoding, dependency, mutation algorithm or reinstall requirement
changes. Graph sidecars remain unresolved: `StoredPropertyValue` is unbounded and the existing
ic-stable-structures 0.7.2 getter extracts the entire encoded value before `Storable::from_bytes`.
`ensure_persistable` checks encoding, not a byte cap; checking size after `get` cannot prove pre-read
admission. Source inspection and a native primitive probe on 2026-09-13 additionally show that node
loading reconstructs all overflow addresses before key search, even for a missing key or key-only
iterator. A value-length accessor after node loading would not bound that preceding work. See the
[sidecar read investigation](../investigations/2026-09-13-sidecar-bounded-read-options.md) for measured
neighbor dependence. The [property-value separation design and experiment](../investigations/2026-09-13-property-value-separation-design.md)
prioritizes bounded B-tree cells with independent large bodies. The native experiment verified on
2026-09-13 06:23:23 UTC +0000 passed two tests over 24 configurations and removed unselected-body
read/write coupling, but the fixed-block/replacement-body candidate is not selected for production
because of short-body waste and high-water. Allocator, Graph directory/codec/workspace and whole-row
write bounds remain unresolved. No production storage change or dependency fork is implemented. A sidecar-owner solution, policy limits, whole-row
mutation/outbox/outcome preflight and retained positive/zero/rejected replay are still required
before bulk edge SET can be exposed.

### 6a. Predicate, ordinal, and early termination are composed at the visitor

`visit_edges` is the canonical composition point for a topology predicate, counting matching rows,
and early termination. A separate `visit_edges_where` or `read_nth_edge_where` API is not required
for the initial implementation. A caller may count only rows that satisfy its predicate and return
`ControlFlow::Break` when the requested matching ordinal is reached:

```rust
let mut matching_ordinal = 0u32;
let mut selected = None;

graph.visit_edges(owner, label, order, |slot, edge| {
    if predicate(&edge) {
        if matching_ordinal == requested_ordinal {
            selected = Some((slot, edge));
            return ControlFlow::Break(());
        }
        matching_ordinal += 1;
    }
    ControlFlow::Continue(())
})?;
```

The requested ordinal in this composition is a **matching ordinal**: a zero-based position among
rows accepted by the caller's predicate. It is not a `BucketEntryPosition` and it is not ADR 0048's
`PairOrdinal`. A third live edge to the same target is identified by counting matching rows with
that target; CounterpartScan remains the owner of the authoritative `PairOrdinal` relation.

If the predicate depends on inline properties, callers use
`visit_edges_with_inline_property` only for the general per-edge path. Property-first execution
must continue to use `visit_inline_property_batches` followed by `visit_edges_at`, so a generic
predicate does not force one inline-property read per edge. Tombstones are not visited and therefore
do not consume a matching ordinal.

### 7. Selected-slot reads remain a first-class capability

Graph predicate execution reads a selected set of logical slots after an inline-property filtering
phase. It must not scan or materialize the full bucket.

The consolidated capability is:

```rust
pub(crate) fn visit_edges_at<B>(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    slots: &[BucketEntryPosition],
    order: OutEdgeOrder,
    visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, LabeledOperationError>;
```

It:

- sorts and deduplicates the requested logical slots according to `order`;
- skips missing and tombstoned slots;
- preserves the dense contiguous-read optimization; and
- does not attach inline properties.

For slab/run buckets, labeled traversal resolves the descriptor once and delegates normalized
slots to `EdgeStore::visit_row_edge_slots`. Only adjacent requested positions share a range read;
isolated selections do not read the whole bucket. Tiny/tree point-state readers keep their native
addressing. Owner, label and orientation are resolved from canonical state on every call.

Property-first execution uses this same capability in phase two. Phase one retains only its
`slot_indices` and `values` buffers; it does not capture an edge replay. Graph joins those bytes to
the returned selected slots. The outgoing and incoming wrappers have the same contract.
`HybridOverflowEdgeReplay`, `visit_edges_at_with_replay`, and the Graph scratch-reusing adapters
are deleted, with no compatibility path or replacement cache.

The borrowed `visit_edges_at_with_inline_property` capability owns selected topology/value
hydration. It sorts and deduplicates requests and emits only canonical live positions. For tree
buckets, it first consumes the canonical point-state decision, then reads the native property
value at that same original logical position through `tree_read::read_property_value_at_slot`.
Tree holes are not converted to slab live ranks. Width-zero trees borrow empty bytes without
property-root access. Raw native bulk visitors retain their separate tombstone-inclusive contract.

For slab/run buckets, initialized extent equal to live degree admits slot-as-live-ordinal mapping.
Otherwise one ascending topology pass computes the selected live ranks, stopping at the largest
requested position; descending output reverses only the selected results. Holes retain their
original logical positions. Adjacent live ordinals share value range reads through
`read_bucket_inline_property_bytes_range_into`. This rank computation can still inspect the sparse
prefix: no persistent rank/select index is introduced.

`ValuesSpillStore::read_range` owns scalar/range width, output and used-length validation. Inline
cells are read in bulk; values wider than eight bytes still require per-site blob-map decoding.
Hybrid property-first admission reads initialized lengths without prefetching edge bodies, and
streaming property batches reuse bounded value ranges rather than reading each value separately.
Physical slab-span reads used by existing dense/maintenance paths retain their distinct contract.

Property bytes remain borrowed only for the callback, and user `ControlFlow::Break` values are
preserved. Selected hydration may prepare topology or value buffers before callbacks; this API does
not promise an error-after-Break ordering across storage modes.

[Range-read implementation and measurements](../evidence/adr0097-selected-hybrid-range-reads.md)
cover the replay removal, exact identities, range IO and remaining wide-value cost.

The bounded tree-selected repair is implemented in `036649e67`; [valid-backed evidence and limits](../evidence/adr0097-selected-tree-properties.md)
cover width8/33 boundaries, original holes, sparse selections, exact root errors and read-only
preservation. It does not enable tree mutation or establish Graph transaction/atomicity safety.

### 7a. Future research: block-aware selected and window traversal

**Planned; not implemented and not part of the current persistence format.**

Current selected topology reads coalesce adjacent requested positions without a heuristic.
Sparse property hydration still counts prefix liveness. A future research slice may partition the logical bucket extent into fixed-size blocks (for example,
16 or 32 slots), group selected slots into block runs, and choose canonical scanning or direct
reads per run. A block directory could skip empty or irrelevant blocks while preserving the
existing logical-slot and tombstone contracts.

The same block partition is also a candidate acceleration structure for `visit_edges_window`.
Selected-slot traversal can use block membership to avoid unrelated ranges; window traversal can
use a per-block live-row count, or a cumulative live-count directory, to skip whole blocks while
consuming `TraversalWindow.offset` across tombstones. The implementation should evaluate one
derived block directory for both access patterns rather than introducing separate selected-slot
and window indexes. Fixed block boundaries alone are insufficient for tombstone-aware offset
skipping: the directory needs live counts or equivalent rank information, and updates, compaction,
and rebuild must keep those counts consistent.

This reuse applies to the physical live-row offset contract. A window whose offset is a
predicate-dependent matching ordinal still has to evaluate that predicate at the row-execution
boundary; generic target or inline-property predicates cannot be skipped using live-row counts
alone. The block design must preserve this distinction and must not silently reinterpret
`TraversalWindow.offset` or the matching-ordinal contract.

This makes physical live-row offset selection a prefix-sum/select problem over block counts. A
hierarchical directory can reduce the number of blocks inspected for large offsets, while a
rank/select representation of the live-bit vector is another candidate when the tombstone map is
large enough to justify its metadata. Succinct indexable dictionaries demonstrate the relevant
rank/select and prefix-sum primitives, but their static-space guarantees do not establish that a
dynamic stable-memory representation is appropriate here
([Raman, Raman, and Satti](https://arxiv.org/abs/0705.0552)). The research must therefore measure
update, compaction, rebuild, and stable-memory costs rather than import the asymptotic result as a
design requirement.

Block-skipping and adaptive set-intersection research supports skipping known-irrelevant ranges,
but does not change the matching-ordinal semantics
([Ding and König](https://www.microsoft.com/en-us/research/publication/fast-set-intersection-in-memory/),
[Barbay, López-Ortiz, and Lu](https://users.dcc.uchile.cl/~jbarbay/Publications/2006-WEA-FasterAdaptiveSetIntersectionsForTextSearching-BarbayLopezOrtizLu.pdf)).

This candidate is inspired by partitioned posting-list and adaptive set-intersection techniques,
including [Fast Set Intersection in Memory](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/vldb11intersection.pdf),
[Faster Adaptive Set Intersections](https://users.dcc.uchile.cl/~jbarbay/Publications/2006-WEA-FasterAdaptiveSetIntersectionsForTextSearching-BarbayLopezOrtizLu.pdf),
and the sequential adjacency-scan design in [LiveGraph](https://arxiv.org/abs/1910.05773). It must
be evaluated against LARA's stable-memory costs rather than comparison counts alone.
The research must compare prefix, suffix, descending, sparse, tombstoned, and mixed slab/overflow
selections. It must not add per-edge counterpart metadata, make derived block state authoritative,
or replace the canonical direct-read fallback without measured runtime, stable-read, rebuild, and
failure-atomicity evidence. Any adopted block directory requires a separate ADR or an explicit
amendment to this ADR.

### 8. Batched inline-property reads remain specialized

The following two execution capabilities remain distinct because they return borrowed batches and
avoid per-edge allocation:

- visit edge rows together with batched inline-property bytes; and
- visit only slot indices and batched inline-property bytes for property-first filtering.

Their target names use `inline_property`, for example:

```rust
visit_edge_inline_property_batches(...)
visit_inline_property_batches(...)
```

The associated batch and scratch types use the same terminology. These APIs select dense, hybrid,
or sparse implementations privately; scratch does not contain a topology replay.

They are not folded into `visit_edges_with_inline_property`: attached per-edge values and borrowed
property batches have different allocation and lifetime contracts.

### 9. Scope and visibility follow ownership

This ADR consolidates **single-label bucket reads**. It does not claim that every read method in
`traverse.rs` is label-scoped. All-label scans and directedness-based scans may remain separate
owner-facing capabilities.

Canonical single-orientation primitives are crate-public only where the bidirectional LARA owner
needs them. Graph callers use forward/reverse methods on the bidirectional wrapper and do not select
an orientation by reaching into the underlying graphs.

The following remain private:

- bucket descriptor lookup;
- slab/log mapping;
- overflow-log chain construction;
- raw tombstone decoding;
- inline-property ordinal mapping (currently implemented by inline-property-bytes-named helpers);
- `labeled_bucket_span_iter`; and
- dense/hybrid/sparse implementation selection.

The LARA edge layer has one canonical **ascending** iterator, `OutEdgesIter`, for both labeled and
unlabeled callers. It is the default traversal (stable materialization order): slab slots low→high,
then reserved overflow-log entries replayed oldest-to-newest. The explicit descending path
(`DescOutEdgesIter`, via `desc_out_edges_iter` / `OutEdgeOrder::Descending`) walks the overflow log
from the chain head first, then live slab slots high→low. The `asc_*`-named aliases were merged
into the unsuffixed defaults and removed (Plan 0195); the legacy `OutEdgeSlabIter` remains the
internal descending slab-only helper for maintenance paths.

### 9a. Raw storage locations have one explicit internal reader

Raw locations are not a second traversal identity. The replacement API is crate-visible only to
LARA maintenance and in-crate adoption/contract tests; it is not re-exported through the Graph,
Router, or graph-index boundaries:

```rust
pub(crate) fn visit_storage_edge_locations<B>(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    visit: impl FnMut(StorageEdgeRef, E) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, LabeledOperationError>;
```

Its contract is fixed and deliberately narrower than `visit_edges`:

- storage order is slab local slot ascending, followed by the bucket's overflow-log chain in
  ascending chain order;
- tombstoned/deleted rows are skipped, and the callback receives only live rows;
- the callback receives a fully contextual `StorageEdgeRef` whose owner and label match the
  selected bucket;
- a missing label bucket produces an empty result, while an out-of-range owner, malformed chain,
  invalid slab/local pair, or location owned by another bucket returns `LabeledOperationError`;
- the high-bit `u32` encoding is decoded at this boundary and never appears in the callback type;
  and
- the only production caller during the replacement is LARA's maintenance/counterpart
  validation path; adoption fixtures use the same crate-visible method and do not define another
  raw-location API.

While adoption remains `not activated`, the existing Plan 0181 crate-private physical-slot reader
and singleton counterpart bridge are a transitional implementation path. They are retained until
the `mate.rs` singleton validation path is migrated to `read_edge(BucketEntryPosition)`. They are not
the target query contract and must not become a Graph, Router, or graph-index identity API.

The existing `read_physical_edge_at_slot_for_label` and
`for_each_live_physical_edge_location_for_label` methods are deleted, not renamed compatibility
surfaces. `mate.rs` uses `read_edge(BucketEntryPosition)` for logical validation; maintenance uses the
reader above only when it must inspect storage geometry.

### 10. Default-label bypass follows the same logical contract

The canonical logical API covers both bucket-mode rows and the default-label bypass:

| Condition                        | Point read | State read  | Visitor/selected read |
| -------------------------------- | ---------- | ----------- | --------------------- |
| owner is out of range            | error      | error       | error                 |
| requested label does not match   | `None`     | `Missing`   | empty                 |
| matching bypass label, live slot | live edge  | `Live`      | emitted               |
| matching bucket label, live slot | live edge  | `Live`      | emitted               |
| tombstoned bucket slot           | `None`     | `Tombstone` | skipped               |
| slot outside the logical extent  | `None`     | `Missing`   | skipped               |

The bypass path must not silently return `Missing` merely because it has no `LabelBucket`
descriptor. The planned projection is explicit:

- the bypass storage label is `bypass_storage_label(default_label)`;
- `stored_slots` is the slab width; slab slot `i` maps to the core span at
  `base_slot_start + i` for `0 <= i < stored_slots`;
- let `overflow_chain_len` be the number of entries reachable from
  `bypass_overflow_log_head()`; the logical extent has length
  `stored_slots + overflow_chain_len` and continues through the overflow-log chain; and
- the extent is ordered by the core traversal contract, not by the numeric encoding of a raw
  slab or log location.

Ascending order visits the slab portion followed by the overflow suffix; descending order is the
exact reverse of that logical order. For example, with one slab row and one overflow entry,
`stored_slots == 1` and `overflow_chain_len == 1`, so logical slot `0` is the slab row and logical
slot `1` is the overflow entry. A slot remains addressable while tombstoned, but a tombstone
does not consume the matching ordinal used by predicate selection. Point/state reads use the core
live/deleted decoding for slab rows and the same tombstone predicate for overflow entries. A
malformed chain, owner mismatch, or impossible slot-to-location mapping is an error (fail closed),
not an empty result.

Default-label bypass has no inline-property-bytes under ADR 0008 (its inline property byte
width is zero). Therefore `*_with_inline_property` on a bypass row is equivalent to the topology
visitor and must not access inline-property storage. Adding bypass inline properties requires a
separate ADR; it must not be inferred from this projection.

Raw storage-location APIs may continue to exclude bypass rows when they are explicitly
measurement-only; that exception must be stated on the method and cannot weaken the logical API
above.

## Alternatives

### A. Minimum change: typed slots plus a small canonical facade

Add `BucketEntryPosition`, keep existing optimized implementations behind the facade, migrate callers,
then remove superseded names.

Benefits:

- fixes the identity ambiguity without changing storage;
- preserves measured dense/hybrid/sparse paths;
- minimizes implementation and benchmark risk; and
- keeps invariant enforcement in LARA.

Costs:

- temporary adapters are required during migration; and
- specialized batch capabilities remain alongside the canonical facade.

**Decision: adopt this alternative.**

### B. Moderate change: one read method with options

Use one method with inline-property attachment, tombstone visibility, ordering, and output-shape
options.

Rejected because the option combinations create invalid or misleading states, obscure allocation,
and make return types depend on runtime flags.

### C. Large redesign: one generic cursor abstraction

Replace point, selected-slot, attached-property, and batched-property reads with a generic cursor.

Rejected because borrowed batches, point states, selected-slot reads, and attached `E` values do
not share one stable lifetime or allocation contract. The abstraction is broader than the
demonstrated problem and would put performance policy into a generic framework.

## Consolidation migration history

The following records the completed consolidation sequence and its original activation gates.
Removed replay/scratch adapters in the map resolve to the canonical selected-slot contract above.
Other ADRs' caller-adoption obligations retain their own status.

Adoption was phased. The temporary module, its tests, and its benchmark module are authoritative for
new work during migration; the legacy `traverse` module remains authoritative only for callers not
yet migrated. The final rename happens only after all callers and validation gates pass.

The concrete `Edge` value split is not a compatibility migration. Topology-only callers use `Edge`,
while property-aware callers use `EdgeWithInlinePropertyRef<'a, Edge>` in the callback and explicitly
clone/materialize only when they need to retain the property. No deprecated owned traversal wrapper,
dual-shape wrapper, or compatibility adapter is retained after the refactor.

ADR 0048 remains the owner of `CanonicalEdgeOccurrence`, `EdgeHandle`, `PairOrdinal`, and counterpart
resolution. ADR 0023 remains the owner of sidecar/index re-key after `EdgeSlotMove`. This ADR only
specifies the read-side conversion boundary: LARA converts storage positions to
`BucketEntryPosition`; Graph, Router, and graph-index convert that typed value to their existing
internal or wire `u32` fields at explicit adapters. The wire types (`GlobalEdgeId`,
`FederatedExpandNeighbor`, `LocalEdgePosting`, and `EdgePostingKey`) are not silently treated as
stable identities.

### Activation dependencies and gates

ADR 0050 depends only on the ADR 0048 **substrate gate**, not on completion of the entire ADR
0048 caller migration. The substrate gate is satisfied when the following contracts exist in the
listed owners:

1. `counterpart.rs` exposes the typed `BucketEntryPosition`, `CanonicalEdgeOccurrence`, `EdgeHandle`,
   `PairOrdinal`, and live-only `CounterpartScan` contracts;
2. paired mutation emits `EdgeSlotMove` through the LARA owner; and
3. the sidecar re-key observer contract is defined, even if legacy `mate` callers and
   `EDGE_ALIASES` removal are still pending.

Once the substrate gate passes, implementation proceeds through the following phases. Each phase
has an independently reviewable result and leaves the tree buildable if later phases are deferred.

#### Phase 0 — inventory and boundary freeze

Inventory every `traverse` caller, including forward/reverse wrappers, selected-slot replay and
scratch paths, offset/skip visitors, `mate.rs`, maintenance readers, tests, and benchmarks. Freeze
the order, short-circuit, inline-property, corruption, and raw-location contracts in this ADR.
No caller is changed in this phase.

#### Phase 1 — isolated logical read module

Add the crate-level `traverse` module with `TraversalRequest`, `Traversal`, and
`traverse::iter` visitor drivers. Keep slab/log decoding in `lara::edge::iter`; the new module must
not depend on storage layout. Connect the existing labeled `traverse_next` implementation through
typed adapters, while the old labeled `traverse` remains authoritative for unmigrated callers.
Add module-local tests for bucket and default-label bypass, slab/overflow mixtures, tombstones,
ascending/descending order, forward and reverse reads, selected slots, inline-property exact bytes,
early break, and corruption fail-closed behavior. At the end of this phase, the common contract and
driver are usable but not yet the crate-wide default.

#### Phase 2 — dedicated benchmark and parity gate

Add a dedicated benchmark module for the new `traverse` surface, covering dense, hybrid, sparse, bypass,
reverse, selected-slot replay, inline-property batches, and early-break paths. During the isolated
substrate phase, persist one deterministic artifact per benchmark family to prove registration and
reproducibility. The strict baseline/candidate comparison is deferred to the activation gate, when
caller migration makes the comparison meaningful; do not repeat an identical canbench invocation as
a noise estimate.

#### Phase 3 — enable ADR 0048 adoption on the new surface

When Phases 1–2 pass, ADR 0048 may proceed with its ordinary-caller adoption work while the old
labeled `traverse` implementation remains in place. This is the non-circular hand-off: ADR 0048's
substrate gate is needed for Phase 1, but completion of ADR 0048 alias removal is not required to
build or validate the new common module. ADR 0048 must use the new logical read surface for any
caller it migrates and must not add a second traversal primitive.

#### Phase 4 — migrate callers incrementally

Route newly written callers to `traverse_next`, then migrate the inventory in bounded groups:
forward/reverse wrappers, Graph predicate scratch/replay and inline-property paths, offset/skip
visitors, and finally the `mate.rs` singleton validation path. Each group keeps its existing order,
OFFSET, callback short-circuit, scratch lifetime, and error semantics, with focused tests and
benchmark evidence before the old path is removed for that group.

#### Phase 5 — remove and rename

After all callers and ADR 0048's required adoption/alias gates pass, delete the old `traverse`
module and rename `traverse_next` to `traverse`. Update module paths, tests, benchmarks, and
documentation in the same change. The rename is a cleanup step, not a second API contract.

Final activation additionally requires the following independently verifiable conditions:

- the two old physical-location methods in the legacy traversal module
  are deleted;
- `mate.rs` singleton validation uses `read_edge(BucketEntryPosition)`;
- the crate-visible `visit_storage_edge_locations` visitor has the contract above and is the only
  raw-location reader used by LARA maintenance or in-crate adoption tests;
- every forward and reverse single-label caller in the migration table is migrated;
- the ADR 0048 alias-removal and pair-order tests pass; and
- the focused Rust, test, and benchmark gates in this ADR pass.

A renamed old method, a partially completed `traverse_next` implementation, a stale
slot/ordinal conflation, or a forward-only migration is not sufficient for activation. ADR 0048
caller adoption and alias removal remain their own gate; they do not block development of the
isolated read module, but both must be complete before final activation of the combined design.

### Migration map

The implementation plan must inventory every caller before changing visibility. The expected
mapping for the known single-label read families is:

| Current family | Target contract |
| --- | --- |
| `for_each_edges_for_label` | `visit_edges` descending |
| `for_each_live_edge_slot_for_label` | `visit_edges` ascending |
| `for_each_live_edge_slot_for_label_desc` | `visit_edges` descending |
| `for_each_edges_for_label_ordered` | `visit_edges_with_inline_property` |
| `for_each_edges_for_label_topology_ordered` | `visit_edges` |
| `out_edges_iter_for_label` | `visit_edges` descending |
| `out_edges_iter_for_label_ordered` | `visit_edges_with_inline_property` |
| `for_each_out_edges_for_label` | forward `visit_edges` descending |
| `for_each_out_edges_for_label_ordered` | forward `visit_edges` with `OutEdgeOrder` |
| `for_each_out_edges_for_label_topology_ordered` | forward `visit_edges` |
| `for_each_in_edges_for_label` | reverse `visit_edges` descending |
| `for_each_in_edges_for_label_ordered` | reverse `visit_edges` with `OutEdgeOrder` |
| `for_each_in_edges_for_label_topology_ordered` | reverse `visit_edges` |
| `read_out_edge_slot_for_label` | `read_edge(BucketEntryPosition)` |
| `read_edge_slot_state_for_label` | `read_edge_state(BucketEntryPosition)` |
| `read_out_edge_slots_for_label` | `visit_edges_at` |
| `read_out_edge_slots_for_label_with_replay` | removed; canonical `visit_edges_at` |
| `read_out_edge_slots_for_label_reusing_inline_property_scratch` | removed; canonical `visit_edges_at`, with phase-one bytes joined by the caller |
| `read_in_edge_slots_for_label` | reverse `visit_edges_at` |
| `read_in_edge_slots_for_label_with_replay` | removed; reverse canonical `visit_edges_at` |
| `read_in_edge_slots_for_label_reusing_inline_property_scratch` | removed; reverse canonical `visit_edges_at`, with phase-one bytes joined by the caller |
| `read_physical_edge_at_slot_for_label` | remove; migrate `mate` to logical read |
| `for_each_live_physical_edge_location_for_label` | physical-location visitor |
| `visit_out_edge_inline_property_batches_for_label` | forward inline-property batch capability |
| `visit_in_edge_inline_property_batches_for_label` | reverse inline-property batch capability |
| `visit_out_inline_property_batches_for_label` | forward inline-property batch capability |
| `visit_in_inline_property_batches_for_label` | reverse inline-property batch capability |
| `visit_*_inline_property_batches_for_*` | inline-property batch capability |
| `skip_then_visit_each_out_edge_for_label`, `skip_then_visit_each_directed_out_edge`, `skip_then_visit_each_undirected_edge` | `visit_edges` with an explicit offset/ordinal skip policy; preserve order, callback short-circuit, and global OFFSET semantics |
| `skip_then_visit_each_in_edge_for_label`, `skip_then_visit_each_directed_in_edge` | reverse `visit_edges` with the same explicit offset/ordinal skip policy and short-circuit contract |

All-label scans, directedness scans, unchecked storage-internal scans, and bidirectional
forward/reverse wrappers require a separate caller decision; they are not renamed mechanically by
this table.

The migration also has to make the following boundary decisions explicit before any old method is
removed:

1. `EdgeHandle` values containing a `BucketEntryPosition` are query-time location keys, not generation-
   checked stable IDs. The existing `EdgeSlotMove` observer path must re-key properties, postings,
   and legacy aliases before compaction is reported complete. Callers must re-resolve after the
   boundary; generation-based stale-handle rejection is not part of this ADR.
2. Default-label reads must use the bypass label, `base_slot_start`, `stored_slots`, and the
   bypass overflow-log head. `stored_slots` is the slab width and the logical extent is
   `stored_slots + overflow_chain_len`; they must cover live, tombstone, wrong-label,
   out-of-range, slab, overflow, and mixed slab/overflow cases in both orders. Bypass
   inline-property reads must prove that no property store is touched.
3. Selected-slot and offset callers are not mechanical renames. The replacement must normalize
   requested slots to the canonical `OutEdgeOrder` (sort then deduplicate), while preserving the
   inline-property scratch/replay lifetime, callback short-circuit, and whether OFFSET is applied
   before or during traversal. The Graph predicate path
   (`read_out_edge_slots_for_label_reusing_inline_property_scratch` and its incoming twin) and all
   `skip_then_visit_each_*` wrappers are activation-gate callers. Input-slice order is not a
   separate contract; an input-order API would require a distinct future capability.
4. Storage-location reads must use `StorageEdgeRef` (not ADR 0048's `CanonicalEdgeOccurrence`) with owner
   and label context, and validate that context against the addressed slab/log owner. The existing
   high-bit `u32` encoding may remain an internal implementation detail only; it is not the public
   raw-location contract. The production `mate.rs` singleton fast path migrates to the logical
   read API in the same replacement.

## Validation

### Contract tests

The test matrix covers:

- bucket mode and default-label bypass;
- matching and non-matching labels;
- missing, live, tombstoned, and out-of-range logical slots;
- dense slab, hybrid slab/log, and sparse/log-backed buckets;
- default-label bypass with `stored_slots == 1` and `overflow_chain_len == 1`, asserting point
  reads for logical slots `0` and `1` plus exact ascending (`A`, `B`) and descending (`B`, `A`)
  visitor order;
- ascending and descending logical order;
- early `ControlFlow::Break`;
- predicate selection with matching ordinal zero, middle, last, and out-of-range;
- third live parallel edge selection, proving matching ordinal is distinct from logical slot;
- selected slots that are unordered, duplicated, missing, or tombstoned;
- inline-property width zero and non-zero;
- `EdgeWithInlinePropertyRef<'a, E>` always exposes the exact byte width and bytes, including a
  zero-width empty slice; an `E` implementation with the default no-op attachment hook cannot
  satisfy this contract by returning topology only;
- non-zero inline-property exact-byte reads from slab and overflow-log rows in both forward and
  reverse buckets, with missing, short, overlong, malformed, width-mismatched, and wrong-ordinal
  payloads failing closed as `LabeledOperationError`; zero-width rows prove that no property read
  occurs;
- attached inline-property bytes versus topology-only rows;
- inline-property-first filtering followed by selected-slot traversal;
- phase-two reads resolve current owner/label state after in-place deletion, without replay;
- selected-run IO excludes unrelated rows and stops before subsequent runs on `Break`;
- dense hybrid property-first admission does not read topology bodies;
- sparse selected-property ranks require at most one prefix pass;
- Graph/LARA slot adapters reject direct use of the reverse-in alias tag, and logical slot values
  remain distinct from encoded raw storage locations;
- a delete/reinsert slot-reuse case proves that slot-only mutation input is rejected, while a
  mutation that re-resolves and validates owner/label/orientation/target updates only the current
  edge's sidecars;
- compaction slot move (for example `2 -> 1`) proving that callers consume the move or re-resolve
  the moved edge, and that `EDGE_PROPERTIES`, posting keys, and legacy aliases are re-keyed
  exactly; the test does not require an old slot-only handle to be rejected or identified after
  the move;
- storage-reference owner/label/leaf mismatch and slab absolute/local-offset mismatch fail closed;
- `read_edge`, `read_edge_state`, and both visitor variants fail with `LabeledOperationError` for
  malformed overflow chains, bucket-owner/label mismatches, and impossible logical-to-physical
  mappings rather than returning `Missing` or an empty visit;
- storage-location visitor visibility, slab-then-overflow order, tombstone skipping, missing-label
  empty result, malformed-location error, and adoption/maintenance caller coverage;
- forward and reverse/incoming wrapper reads with identical logical-slot, order, tombstone, and
  early-break behavior;
- default-label bypass point and visitor reads for slab, overflow, live, tombstone, wrong-label,
  out-of-range, ascending, and descending cases, including the zero-width inline-property rule;
- parallel edges used by CounterpartScan; and
- compile-time separation between `BucketEntryPosition`, ADR 0048's `CanonicalEdgeOccurrence`, and
  `StorageEdgeLocation`.

Tests must assert that topology-only reads do not access inline-property storage where the existing
test instrumentation can observe that distinction.

### Benchmark gates

Focused canbench comparisons cover at least:

- dense labeled scan;
- reverse/incoming labeled scan;
- mixed-label ascending and descending scan;
- overflow-log scan;
- CounterpartScan point/rank-select paths;
- selected-slot dense and hybrid range paths; and
- edge-attached and property-first batched inline-property paths.

The migration must not introduce full-bucket allocation into streaming or selected-slot paths.
Each activation gate records the affected crate, benchmark name/pattern, git revision, Rust/profile/features,
workload parameters, machine/runtime identity, command line, and persisted baseline artifact. During
the isolated substrate phase, a persisted deterministic artifact is sufficient to prove that the
new benchmark surface is registered and reproducible. At activation, each comparison uses one
baseline run and one candidate run with exactly the same pattern and build settings; repeating
identical runs is not a noise-reduction method. A regression greater than 5% versus the checked-in
baseline requires investigation and an explicit justification in this ADR. Final benchmark
artifacts are updated only by running the affected crate's unfiltered `canbench --persist`.

## Consequences

Positive:

- slot domains become impossible to mix accidentally;
- LARA remains the source of truth for logical-to-physical mapping;
- CounterpartScan receives an explicit logical-slot contract;
- topology and inline-property reads have unambiguous costs;
- selected-slot and batched reads preserve current execution performance; and
- callers materialize only when they explicitly choose to collect a visitor.

Costs:

- `BucketEntryPosition` propagates through LARA and Graph-facing internal types;
- the inline-property naming migration touches APIs, types, tests, benchmarks, and documentation;
- inline-property reads return an explicit wrapper/byte buffer rather than relying on an optional
  edge-record mutation hook;
- specialized batch APIs remain because their lifetime and allocation contract is genuinely
  different; and
- ADR 0048 must be synchronized with the clarified slot terminology.

## Design documentation impact

The implementation patch must check and update:

- ADR 0048, preserving `CanonicalEdgeOccurrence` as the canonical adjacency occurrence and keeping raw
  storage locations separate;
- ADR 0008, confirming that default-label bypass rows have zero inline-property width;
- ADR 0023, confirming the move/re-key obligations for property, posting, and alias sidecars;
- `design/storage/labeled-edge-inline-properties.md`, when its terminology is renamed;
- `design/storage/lara.md`, for the canonical labeled read boundary;
- `design/storage/lara-and-facade.md`, if facade names or visibility change; and
- Graph execution documentation for property-first selected-slot reads.

This ADR is now aligned with the implemented code. Historical `inline_property`/`payload`
identifiers in older revisions have been superseded by ADR 0051.

## Out of scope

- Write APIs, insertion results, removal, and batch mutation contracts.
- Slab, overflow-log, or inline-property persistence layout changes.
- Persisted counterpart metadata.
- A generic cursor framework.
- Renaming the entire property model in this ADR alone.

## Related

- ADR 0048: LARA-owned counterpart resolution
- ADR 0001: labeled segment slide and PMA ownership
- ADR 0016: overflow-log tombstones and source fields
