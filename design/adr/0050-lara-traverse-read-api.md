# 0050. LARA labeled traverse read API consolidation

Date: 2026-07-25
Status: planned
Implementation status: not started (API integration over the existing traversal substrate)
Adoption status: not activated

## Context

`crates/ic-stable-lara/src/labeled/graph/traverse.rs` has grown an ad-hoc collection of labeled
bucket read APIs. The current surface mixes:

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
- the `visit_*_inline_value_batches_for_*` family.

The duplication makes it difficult to identify the cheapest correct primitive. More importantly,
several APIs use `u32` for two different slot domains, and the word `topology` currently means
"live edge row without inline-value reads" in some call sites rather than "raw stored cell".

The property-model terminology is also changing. The target term is **inline property**. Existing
implementation identifiers such as `inline_value`, `payload`, and
`with_stored_inline_value_bytes` remain the current code vocabulary until the separate naming
migration lands. New public or crate-public read APIs defined by this ADR use `inline_property`;
internal adapters may temporarily call existing inline-value storage helpers.

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

The current specialized selected-slot and batched readers are not accidental duplication. They
preserve dense bulk reads, hybrid replay reuse, inline-property-first predicate execution, and
bounded scratch allocation. Those capabilities remain LARA-owned and must not be expressed as a
generic full-bucket `Vec`.

## Decision

### 1. Distinguish logical identity from raw physical location

The canonical labeled read surface uses a typed bucket-local logical slot:

```rust
#[repr(transparent)]
pub struct LogicalEdgeSlot(u32);
```

`LogicalEdgeSlot` is:

- the slot returned by canonical labeled scans;
- the slot stored in `EdgeHandle`;
- the slot accepted by CounterpartScan; and
- the position within the bucket's logical extent, including tombstoned positions, that LARA
  remaps across slab and overflow-log storage;
- not a compacted live ordinal: tombstones remain addressable at their logical position and do not
  get removed from this address space; and
- distinct from ADR 0048's `PairOrdinal`, which is computed only over the currently live
  equal-target subsequence.

`LogicalEdgeSlot` is a **query-time location**, not a stable logical edge identifier. It is valid
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

No read API may retain a `LogicalEdgeSlot` across that move boundary. If a caller needs the edge
after compaction, it must consume the moved handle or re-resolve it from canonical adjacency. The
move path, rather than a generation catalog or second slot-to-edge catalog, remains the source of
truth.

### 1.1 Existing slot types and storage-key boundaries

`LogicalEdgeSlot` is the LARA-owned slot type because `ic-stable-lara` must not depend on
`gleaph-graph-kernel`. At the Graph boundary, the existing
[`EdgeSlotIndex`](../../crates/graph-kernel/src/entry/edge/id.rs) remains the Graph-facing wrapper
for the same `(owner, label, logical extent position)` value. The adapter between these types is
explicit and is the only place where a raw `u32` may cross the boundary. Graph `EdgeHandle` must
not retain an untyped `u32` slot field after the migration.

The reverse-in alias tag (`0x8000_0000`) is a storage-key encoding, not part of the logical-slot
namespace. A `LogicalEdgeSlot` must never be passed directly to that encoder. While aliases remain
in the transitional implementation, alias keys use an explicit `(EdgeSlotIndex, direction)`
adapter/key representation; no `LogicalEdgeSlot` range restriction is introduced merely to fit the
legacy high-bit encoding. Alias removal is still governed by ADR 0048.

ADR 0048's phrase "physical edge identity" means the identity of one persisted adjacency occurrence
at the LARA boundary. It does **not** mean a raw slab offset or overflow-log entry index.

The existing `PhysicalEdgeRef` name belongs to ADR 0048 and remains the logical counterpart
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

`StorageEdgeLocation` is not a substitute for `LogicalEdgeSlot` or ADR 0048's `PhysicalEdgeRef`.
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

Attaching inline property bytes changes the effective `E` value and performs additional storage
reads. The canonical API therefore uses separate methods and a return type that makes the bytes
observable without relying on an optional `CsrEdge` hook:

```rust
pub struct InlinePropertyBytes {
    pub width: u16,
    pub bytes: Vec<u8>,
}

pub struct EdgeWithInlineProperty<E> {
    pub edge: E,
    pub inline_property: InlinePropertyBytes,
}
```

LARA constructs `InlinePropertyBytes` and enforces `bytes.len() == width`. Width zero is a valid
value and is represented by an empty byte vector. `CsrEdge::with_stored_inline_value_bytes` may be
used as an internal optimization, but it is not the contract and its default no-op implementation
must never be able to hide a missing property read.

For a non-zero width, the bytes must be read from the exact inline-property ordinal belonging to
the live row identified by the requested logical slot. This identity check applies independently
to slab and overflow-log storage and to forward and reverse buckets. A missing row, short or
overlong payload, malformed payload, width mismatch, or bytes belonging to another ordinal is a
`LabeledOperationError`; implementations must not zero-fill, borrow an adjacent row, or downgrade
the condition to `Missing`. Width zero performs no property read and returns an empty byte vector.

- names without a suffix return live topology only; and
- names ending in `_with_inline_property` return `EdgeWithInlineProperty<E>` for each live row.

Boolean `attach_payload`/`attach_inline_property` flags are private implementation details and are
not exposed in the consolidated surface.

The target term `inline_property` is used even while the implementation delegates to existing
`inline_value` helpers. `payload` is not used in new API names because it is broader and can be
confused with other edge payload or property-store concepts.

### 4. Order is an argument

Ascending and descending traversal use `OutEdgeOrder`. Direction is not encoded in method names.

The order applies to `LogicalEdgeSlot`, including edges whose bytes currently reside in an overflow
log. Raw physical-location enumeration has its own storage-defined order and is not covered by
`OutEdgeOrder`.

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

The smallest general-purpose surface is:

```rust
pub(crate) fn read_edge(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    slot: LogicalEdgeSlot,
) -> Result<Option<E>, LabeledOperationError>;

pub(crate) fn read_edge_with_inline_property(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    slot: LogicalEdgeSlot,
) -> Result<Option<EdgeWithInlineProperty<E>>, LabeledOperationError>;

pub(crate) fn read_edge_state(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    slot: LogicalEdgeSlot,
) -> Result<EdgeSlotState<E>, LabeledOperationError>;

pub(crate) fn visit_edges<B>(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    order: OutEdgeOrder,
    visit: impl FnMut(LogicalEdgeSlot, E) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, LabeledOperationError>;

pub(crate) fn visit_edges_with_inline_property<B>(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    order: OutEdgeOrder,
    visit: impl FnMut(LogicalEdgeSlot, EdgeWithInlineProperty<E>) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, LabeledOperationError>;
```

There is no separate full-bucket `Vec` API. A caller that needs materialization collects
`visit_edges` output explicitly, making the allocation visible at the caller.

All logical readers share the same corruption boundary. A missing label, an out-of-range logical
slot, or a tombstone is a normal absence/state result according to the method contract. In
contrast, a malformed overflow chain, bucket-owner or label mismatch, an impossible
logical-slot-to-physical mapping, or an inline-property ordinal mismatch is
`LabeledOperationError` for `read_edge`, `read_edge_with_inline_property`, `read_edge_state`,
`visit_edges`, and `visit_edges_with_inline_property`. These conditions must never be represented
as `None`, `Missing`, an empty visit, or fabricated property bytes.

There is no `read_edge_state_with_inline_property`: inline property bytes exist only for a live
row, so callers first inspect `read_edge_state` and call `read_edge_with_inline_property` only when
needed. A measured point-read caller may justify a fused operation later. The returned wrapper,
not `E`, is the proof that the inline-property read occurred.

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
rows accepted by the caller's predicate. It is not a `LogicalEdgeSlot` and it is not ADR 0048's
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
    slots: &[LogicalEdgeSlot],
    order: OutEdgeOrder,
    visit: impl FnMut(LogicalEdgeSlot, E) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, LabeledOperationError>;
```

It:

- sorts and deduplicates the requested logical slots according to `order`;
- skips missing and tombstoned slots;
- preserves the dense contiguous-read optimization; and
- does not attach inline properties.

Selected reads that need the existing two-phase payload optimization use a separate capability and
do not allocate one payload value per edge:

```rust
pub(crate) fn visit_edges_at_with_replay<B>(
    &self,
    owner: VertexId,
    label: BucketLabelKey,
    slots: &[LogicalEdgeSlot],
    order: OutEdgeOrder,
    replay: Option<&HybridOverflowEdgeReplay>,
    visit: impl FnMut(LogicalEdgeSlot, E) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, LabeledOperationError>;
```

`HybridOverflowEdgeReplay` is the existing opaque snapshot carried by
`LabeledPayloadValueBatchScratch`. It is accepted only when its owner, label, slab/log split, and
bucket snapshot match; otherwise the implementation discards it and performs the canonical sparse
read. The replay is borrowed for the call and never becomes a source of truth. Inline-property
bytes are attached by the dedicated property-first phase using the same scratch's `slot_indices`
and `values` buffers; the selected visitor consumes those bytes without allocating per-edge
payload buffers. The reverse wrapper has the identical contract with reverse orientation.

Replay/scratch reuse remains an optimized LARA capability. The replay object is opaque, proves its
`(owner, label, bucket fingerprint)` before use, and falls back to canonical reads on mismatch. It
must not become a second source of truth.

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

The associated batch, scratch, and replay types follow the same terminology when the naming
migration lands. These APIs may select dense, hybrid, or sparse implementations privately.

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
- inline-property ordinal mapping (currently implemented by payload-named helpers);
- `labeled_bucket_span_iter`; and
- dense/hybrid/sparse implementation selection.

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

While this ADR remains `not started`, the existing Plan 0181 crate-private physical-slot reader
and singleton counterpart bridge are a transitional implementation path. They are retained until
the `mate.rs` singleton validation path is migrated to `read_edge(LogicalEdgeSlot)`. They are not
the target query contract and must not become a Graph, Router, or graph-index identity API.

The existing `read_physical_edge_at_slot_for_label` and
`for_each_live_physical_edge_location_for_label` methods are deleted, not renamed compatibility
surfaces. `mate.rs` uses `read_edge(LogicalEdgeSlot)` for logical validation; maintenance uses the
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

Default-label bypass has no inline-property payload under ADR 0008 (its payload/inline-property
width is zero). Therefore `*_with_inline_property` on a bypass row is equivalent to the topology
visitor and must not access inline-property storage. Adding bypass inline properties requires a
separate ADR; it must not be inferred from this projection.

Raw storage-location APIs may continue to exclude bypass rows when they are explicitly
measurement-only; that exception must be stated on the method and cannot weaken the logical API
above.

## Alternatives

### A. Minimum change: typed slots plus a small canonical facade

Add `LogicalEdgeSlot`, keep existing optimized implementations behind the facade, migrate callers,
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

Rejected because borrowed batches, point states, selected-slot replay, and attached `E` values do
not share one stable lifetime or allocation contract. The abstraction is broader than the
demonstrated problem and would put performance policy into a generic framework.

## Migration

Adoption is phased. The temporary module, its tests, and its benchmark module are authoritative for
new work during migration; the legacy `traverse` module remains authoritative only for callers not
yet migrated. The final rename happens only after all callers and validation gates pass.

ADR 0048 remains the owner of `PhysicalEdgeRef`, `EdgeHandle`, `PairOrdinal`, and counterpart
resolution. ADR 0023 remains the owner of sidecar/index re-key after `EdgeSlotMove`. This ADR only
specifies the read-side conversion boundary: LARA converts storage positions to
`LogicalEdgeSlot`; Graph, Router, and graph-index convert that typed value to their existing
internal or wire `u32` fields at explicit adapters. The wire types (`GlobalEdgeId`,
`FederatedExpandNeighbor`, `LocalEdgePosting`, and `EdgePostingKey`) are not silently treated as
stable identities.

### Activation dependencies and gates

ADR 0050 depends only on the ADR 0048 **substrate gate**, not on completion of the entire ADR
0048 caller migration. The substrate gate is satisfied when the following contracts exist in the
listed owners:

1. `counterpart.rs` exposes the typed `LogicalEdgeSlot`, `PhysicalEdgeRef`, `EdgeHandle`,
   `PairOrdinal`, and live-only `CounterpartScan` contracts;
2. paired mutation emits `EdgeSlotMove` through the LARA owner; and
3. the sidecar re-key observer contract is defined, even if legacy `mate` callers and
   `EDGE_ALIASES` removal are still pending.

Once the substrate gate passes, implementation may start in an isolated replacement module. The
implementation and validation sequence is:

1. Add a temporary `traverse_next` module beside the existing `traverse` module. Implement the
   ADR 0050 API there, with module-local tests covering bucket/bypass, tombstones, inline
   properties, selected slots, forward/reverse reads, and corruption boundaries.
2. Add a dedicated traversal benchmark module for the new surface. Benchmark fixtures must cover
   dense, hybrid, sparse, bypass, selected-slot, and early-break paths before caller migration.
3. Route newly written callers to `traverse_next`; existing callers may continue using `traverse`
   while parity and benchmark gates are evaluated.
4. Migrate existing forward/reverse `traverse` callers, Graph predicate scratch/replay paths,
   offset wrappers, and the `mate.rs` singleton validation path to `traverse_next`.
5. Delete the old `traverse` module and rename `traverse_next` to `traverse`, then update tests,
   benchmarks, and documentation. The rename is a cleanup step, not a second API contract.

Final activation additionally requires the following independently verifiable conditions:

- the two old physical-location methods in the legacy traversal module
  are deleted;
- `mate.rs` singleton validation uses `read_edge(LogicalEdgeSlot)`;
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
| `read_out_edge_slot_for_label` | `read_edge(LogicalEdgeSlot)` |
| `read_edge_slot_state_for_label` | `read_edge_state(LogicalEdgeSlot)` |
| `read_out_edge_slots_for_label` | `visit_edges_at` |
| `read_out_edge_slots_for_label_with_replay` | `visit_edges_at` plus replay/scratch |
| `read_out_edge_slots_for_label_reusing_inline_value_scratch` | `visit_edges_at` plus the explicit inline-property scratch/replay capability; canonicalize slots by `OutEdgeOrder` and preserve payload attachment |
| `read_in_edge_slots_for_label` | reverse `visit_edges_at` |
| `read_in_edge_slots_for_label_with_replay` | reverse `visit_edges_at` plus replay/scratch |
| `read_in_edge_slots_for_label_reusing_inline_value_scratch` | reverse `visit_edges_at` plus the explicit inline-property scratch/replay capability; canonicalize slots by `OutEdgeOrder` and preserve payload attachment |
| `read_physical_edge_at_slot_for_label` | remove; migrate `mate` to logical read |
| `for_each_live_physical_edge_location_for_label` | physical-location visitor |
| `visit_out_edge_inline_value_batches_for_label` | forward inline-property batch capability |
| `visit_in_edge_inline_value_batches_for_label` | reverse inline-property batch capability |
| `visit_out_inline_value_batches_for_label` | forward inline-property batch capability |
| `visit_in_inline_value_batches_for_label` | reverse inline-property batch capability |
| `visit_*_inline_value_batches_for_*` | inline-property batch capability |
| `skip_then_visit_each_out_edge_for_label`, `skip_then_visit_each_directed_out_edge`, `skip_then_visit_each_undirected_edge` | `visit_edges` with an explicit offset/ordinal skip policy; preserve order, callback short-circuit, and global OFFSET semantics |
| `skip_then_visit_each_in_edge_for_label`, `skip_then_visit_each_directed_in_edge` | reverse `visit_edges` with the same explicit offset/ordinal skip policy and short-circuit contract |

All-label scans, directedness scans, unchecked storage-internal scans, and bidirectional
forward/reverse wrappers require a separate caller decision; they are not renamed mechanically by
this table.

The migration also has to make the following boundary decisions explicit before any old method is
removed:

1. `EdgeHandle` values containing a `LogicalEdgeSlot` are query-time location keys, not generation-
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
   (`read_out_edge_slots_for_label_reusing_inline_value_scratch` and its incoming twin) and all
   `skip_then_visit_each_*` wrappers are activation-gate callers. Input-slice order is not a
   separate contract; an input-order API would require a distinct future capability.
4. Storage-location reads must use `StorageEdgeRef` (not ADR 0048's `PhysicalEdgeRef`) with owner
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
- `EdgeWithInlineProperty<E>` always exposes the exact byte width and bytes, including a zero-width
  empty value; an `E` implementation with the default no-op attachment hook cannot satisfy this
  contract by returning topology only;
- non-zero inline-property exact-byte reads from slab and overflow-log rows in both forward and
  reverse buckets, with missing, short, overlong, malformed, width-mismatched, and wrong-ordinal
  payloads failing closed as `LabeledOperationError`; zero-width rows prove that no property read
  occurs;
- attached inline-property bytes versus topology-only rows;
- inline-property-first filtering followed by selected-slot traversal;
- stale replay rejection and canonical fallback;
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
- compile-time separation between `LogicalEdgeSlot`, ADR 0048's `PhysicalEdgeRef`, and
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
- selected-slot dense and hybrid replay paths; and
- edge-attached and property-first batched inline-property paths.

The migration must not introduce full-bucket allocation into streaming or selected-slot paths.
Each gate records the affected crate, benchmark name/pattern, git revision, Rust/profile/features,
workload parameters, machine/runtime identity, command line, and persisted baseline artifact. Since
canbench instruction counts are deterministic for a fixed code and condition, each comparison uses
one baseline run and one candidate run with exactly the same pattern and build settings; repeating
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

- `LogicalEdgeSlot` propagates through LARA and Graph-facing internal types;
- the inline-property naming migration touches APIs, types, tests, benchmarks, and documentation;
- inline-property reads return an explicit wrapper/byte buffer rather than relying on an optional
  edge-record mutation hook;
- specialized batch APIs remain because their lifetime and allocation contract is genuinely
  different; and
- ADR 0048 must be synchronized with the clarified slot terminology.

## Design documentation impact

The implementation patch must check and update:

- ADR 0048, preserving `PhysicalEdgeRef` as the logical counterpart occurrence and keeping raw
  storage locations separate;
- ADR 0008, confirming that default-label bypass rows have zero inline-property width;
- ADR 0023, confirming the move/re-key obligations for property, posting, and alias sidecars;
- `design/storage/labeled-edge-inline-values.md`, when its terminology is renamed;
- `design/storage/lara.md`, for the canonical labeled read boundary;
- `design/storage/lara-and-facade.md`, if facade names or visibility change; and
- Graph execution documentation for property-first selected-slot reads.

Until implementation lands, this ADR is intentionally ahead of the code. Existing
`inline_value`/`payload` identifiers remain implemented terminology and are not evidence that the
new API has been adopted.

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
