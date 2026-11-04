# Property value separation: bounded directory and independent bodies

Status: Experimental — body separation demonstrated; the fixed-block/replacement-body candidate is not selected for production.

Design anchor: 2026-09-13 03:57:44 UTC +0000.
Probe verified: 2026-09-13 06:23:23 UTC +0000.
Repository anchor: `95d791087`, including the uncommitted LARA read prerequisites.

Follow-up status (2026-09-17 22:24:02 UTC +0000):
[whole versus chunked B-tree bodies](2026-09-13-property-body-btree-comparison.md) were measured before
custom allocation. The [extent experiment](2026-09-13-property-body-extent-design.md) subsequently
completed native and [managed IC comparisons](2026-09-13-property-body-extent-canbench.md).
The [native PropertyStore comparison](2026-09-13-property-store-native-comparison.md) now includes
actual keys/Value, a persisted directory and single-property moves. The separate
[managed full-store IC comparison](2026-09-17-property-store-managed-comparison.md) is also complete.
It compares
B-tree-based body storage with custom extents, not their free-index algorithms. Fine bucket/read
optimizations are deferred. None of these checkpoints selects a production layout, and the fixed-
block results below remain evidence only for their completed, unselected candidate.

## 1. Decision boundary

Design value separation inside Graph's existing PropertyStore owners before carrying a read-budget
fork of the B-tree dependency. Keep the existing per-property keys and ordered enumeration. Keep
small encoded values in the directory and keep unbounded complete bodies outside its B-tree nodes.
The follow-up compares storing capped body chunks in a separate B-tree; it does not require all
body storage to avoid B-trees.

This selects a design investigation, **not** a production codec, threshold, allocator, MemoryId or
activation. The concrete format below exists only in the isolated experiment. Its results in
[section 8](#8-probe-results-and-disposition) stop production adoption of this candidate; they do not
authorize a second allocator or a dependency fork. The separation direction remains worth pursuing.

The [0.7.2 investigation](2026-09-13-sidecar-bounded-read-options.md) owns the baseline evidence:
node-level overflow discovery depends on an unselected body's size, and node saving loads/rewrites
all values in that node. Merely rejecting after `get` does not solve either dependency.

## 2. Ownership and preserved behavior

- [VertexPropertyStore](../../crates/graph/src/facade/stable/vertex_properties.rs) and
  [EdgePropertyStore](../../crates/graph/src/facade/stable/edge_properties.rs) remain the owners of
  property presence, retrieval, replacement, removal, ordered enumeration and physical-key moves.
  Their key encodings remain eight and fourteen bytes respectively. They already store one
  property per entry; the redesign separates bodies, not logical property identity.
- One private cell codec/body implementation is shared by those two existing owners, with a
  separate body-storage instance for each store. No shared mutable cross-store allocator, cross-store
  references or new canister is needed. This is one implementation with two disjoint data sets,
  not two definitions of the property schema.
- The GQL Value binary codec and IC extension decoder keep owning value meaning. The storage cell
  only decides where those encoded bytes live. Small-cell storage is **not** LARA edge INLINE:
  properties do not move into adjacency or acquire an INLINE profile.
- Router, portable parser/planner, Property Index and Vector Index do not see body references.
  Canonical changes continue through the existing Graph fence and derived outbox. No second journal,
  property-length map, lifetime edge ID, reference registry or revision counter is introduced.
- Public behavior remains replacement, not merge; absence differs from a stored NULL. Preserve
  all Value variants, entity/label/slot scope and parallel-edge distinction.

```text
vertex directory: (vertex, property) ─┬─ Small(encoded Value bytes)
                                    └─ External(body reference, encoded length)
                                              │
                                    vertex-owned body storage

edge directory: (owner, label, slot, property) ─┬─ Small(encoded Value bytes)
                                               └─ External(body reference, encoded length)
                                                          │
                                                edge-owned body storage
```

Separate trees per property name are not the selected approach: large and small values of the same
name would still interact, and entity-wide enumeration would lose its existing contiguous key range.

## 3. Candidate cell, not a production wire commitment

Let `b` be the length of the encoded Value bytes, excluding the existing store's version byte.
Experiment thresholds are `T ∈ {32, 64, 128}` bytes. Choose one threshold only after measurements;
these are not user settings or production mode flags.

| Form | Candidate bytes | Encoded cell size |
| --- | --- | ---: |
| Small, `b <= T` | format byte `2`, kind byte `0`, exactly `b` payload bytes | `2 + b` |
| External, `b > T` | format byte `2`, kind byte `1`, head block `u64 LE`, encoded length `u32 LE` | 14 |

`Storable::BOUND` for the cell is `Bounded { max_size: max(T+2,14), is_fixed_size: false }`.
Do not pad every small value to T. The outer B-tree's record length supplies the small form's extent;
there is no duplicated small-length field. The external cell is the sole authority for body length.
A body does not embed another copy of that length.

Reject unknown format/kind, invalid external extent, wrong exact external-cell size, small bodies
larger than T, or external bodies at/below T. Check all integer conversions/address arithmetic.
Retain the existing representability condition `b + 1 <= u32::MAX` rather than silently changing the
ordinary Value-size contract. The experiment uses raw encoded-byte specimens; real Value validity
and decoding allocations remain an integration gate.

This cell is not representable by the implemented `StoredPropertyValue::V1(Value)` decoder. Adoption
would replace that envelope and its callers; it must not add an old/new decoder branch.

## 4. Body allocation: narrow experimental candidate

### Existing-owner check

| Existing component | Why it is not a drop-in bounded body store |
| --- | --- |
| LARA `FreeSpanStore` / byte slab | Reuses ranges, but bin selection may walk many spans; reopen validates the whole free-span index and mutation can rebuild a transient heap. Its release reservation is crate-private. Reuse would require a bounded allocation contract and boundary work, not merely renaming edge slots to bytes. |
| LARA inline-property blob map | Another unbounded-value B-tree and a u16 body limit: reproduces the node coupling and narrows ordinary property values. |
| LARA LTB block store | Private adjacency owner/label/kind/tombstone layout, not a general property allocator. Do not expose adjacency internals or copy that format into Graph. |
| Vector page store | Owns vector row identity, run tables and distance-kernel geometry; using it for generic values would mix data owners. |
| StableLog / append-only body region | Cannot by itself reclaim repeated replacements/deletes. No append-only prototype may be presented as production storage efficiency. |

Source owners: [free_span.rs](../../crates/ic-stable-lara/src/lara/edge/free_span.rs),
[edge_inline_property.rs](../../crates/ic-stable-lara/src/lara/edge_inline_property.rs),
[blobs.rs](../../crates/ic-stable-lara/src/lara/edge_inline_property/blobs.rs),
[LTB](../../crates/ic-stable-lara/src/labeled/ltb_raw_block_store.rs),
[vector pages](../../crates/ic-stable-vector-page-store/src/lib.rs).

### Fixed-block candidate

Test one simple reclaimable chain allocator, private to the diagnostic artifact. Total block sizes
are `Q ∈ {128, 256, 1024, 4096}` bytes; a block contains a 16-byte header and `P = Q-16` body bytes.
Each arena uses one Q and T fixed at creation. Do not implement size classes, best-fit search,
coalescing, compaction, compression or shared-body deduplication in this first experiment.

The candidate arena header is 64 bytes:

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 3 | magic `PVB` |
| 3 | 1 | format version `1` |
| 4 | 4 | Q, u32 LE |
| 8 | 2 | T, u16 LE |
| 10 | 6 | zero reserved |
| 16 | 8 | minted block count, u64 LE |
| 24 | 8 | free-list head, u64 LE |
| 32 | 8 | free block count, u64 LE |
| 40 | 24 | zero reserved |

Block IDs start at 1; zero terminates chains. Block i starts at `64 + (i-1)*Q`. Its header contains
kind `0=Free` / `1=Live` in byte 0, seven zero reserved bytes, then next-block u64 LE at byte 8.
The next pointer means free successor or body successor according to kind. These are private reusable
physical locations, not lifetime IDs and not references retained by Router or across await.

A body uses exactly `ceil(b/P)` Live blocks, all but the final one full, with zero after the final
block. Reads validate IDs/kinds and exact chain termination and never publish a partial result.
Reopen validates the fixed header/configuration/envelope; it does not scan every body or rebuild a
heap free list. Both memory regions must be uninitialized (`size() == 0`) for creation, or both
initialized for reopen; an initialized directory/arena may have no entries/live bodies. A missing
region is rejected before initialization writes; it must not become a fresh arena that reuses block
IDs still referenced by the surviving directory. Free-list safety is checked on touched allocation
paths, including repeated IDs in a prepared allocation. This is not a guarantee against every arbitrary stable-memory corruption.

Allocation uses reusable blocks first and appends only the shortfall. Deletion/shortening returns
old blocks to the persistent free list. Work follows the selected old/new body block counts, not
all free spans or unrelated bodies. Reuse of sufficient freed capacity must survive reopen.

This is deliberately a **measurement baseline**, not a claim that fixed blocks are space-optimal.
Near-threshold bodies can waste most of a block; small Q increases links and reads for large bodies.
If no Q fits the workload, stop and compare a bounded variable-span owner or narrowly justified size
classes in a separate design revision. Do not promote a wasteful 4 KiB-per-short-string layout.

## 5. Read and write contracts

These are required integration contracts. The probe implements raw body admission and local
storage lifecycle checks, not Graph directory-work metering, Value decoding limits, whole-row
preflight or IC rollback.

### Reads

1. Admit bounded directory-probe work before lookup. A directory value can include up to T small
   bytes; this is not a promise that zero bytes are read before the external length is known.
2. Small values need no arena read. For External, validate and charge the encoded length and block
   work **before** body-buffer allocation or the first body access.
3. Read only that body's admitted chain, then decode using the existing Value codec. A separate
   decoding/workspace allowance is required; generic Storable or binary Value decoding is not
   automatically heap-bounded by encoded length.

Bounding the cell removes arbitrary neighbor-body size from valid node payloads. It does not make
B-tree search, key enumeration, memory-manager address translation or insertion free or constant
cost. Tree height/cardinality, directory node packing and write/rebalance work still need explicit
bounds before Graph bulk-row admission can rely on this store. `Bounded` alone is not a pre-read cap
for arbitrarily corrupt upstream encoded-length metadata.

### Replace / remove / move

Prepare under one exclusive local store access, with no intervening mutation/await:

- Validate new bytes, read/decode any required previous values, inspect the complete old body chain,
  plan free-block reuse/append, and account for directory changes, output and scratch space.
- Reject deterministic limits/invalid input before allocator or canonical writes. Capacity
  reservation is not publication; account for backing growth separately from logical data changes.
- Once the first allocator/body/directory write occurs, this is a committing segment. For external
  replacements, publish a fully written new body before its directory reference, then retire the
  old body. No recoverable Err after that
  first write. Unexpected failures must trap in Graph's no-external-call segment; native panic
  catching is not IC rollback proof.
- For whole-row SET, all keys must be prepared before the first canonical write, with duplicate-key
  effects resolved and old references retired only once. A sequence of individually fallible setters
  is not whole-row preflight. Existing index-build admission/outbox/journal work belongs in the same
  Graph segment, not in this storage allocator.

Representation transitions Small→Small, Small→External, External→Small and External→External all
preserve old-value return behavior. Begin with replacement bodies rather than in-place mutation;
measure temporary old+new capacity. In-place updates are a separate optimization, not assumed safe.

**Edge-slot movement transfers ownership, not payload identity.** Move directory entries while
retaining their body references. Do not call the old `remove_all_for_edge` then `set` sequence: the
new removal would free bodies that the move still needs. Preserve the existing destination-property
replacement behavior and retire any displaced destination body exactly once; same-slot moves stay
no-ops. Preserve returned values for derived-index maintenance. That caller may still need to read
bodies; metadata-only rekey does not imply an I/O-free Graph movement.

No reference counting, cross-property body sharing, cross-message prepared handles or durable
allocator recovery journal is needed for this local ownership model.

## 6. Storage accounting and acceptance evidence

For N small values and large-body lengths `b_j`, candidate logical storage is:

- Unchanged existing property keys.
- Directory values: `Σsmall(2+b) + 14 * number_of_large_values`.
- Live arena slots: `Σlarge Q * ceil(b_j/(Q-16))`, plus one 64-byte header per arena.
- Additional reusable/high-water slots, B-tree node/header/free-page overhead and virtual-memory
  allocation rounding must be measured separately.

Compared with the existing `1+b` envelope, a small cell adds **one encoded byte**, not T bytes.
This is not a prediction of total physical growth: the installed B-tree constructor derives page
size from the maximum node size when both types are bounded, whereas an unbounded type selects
its default page size. Directory sizing/occupancy must therefore be part of the comparison.
Large-value cost includes link headers and tail slack, not just the 14-byte reference. For example,
at T=64, a 65-byte body costs 142 bytes with Q=128 or 4110 bytes with Q=4096, versus the existing
66-byte envelope, before either design's B-tree overhead. These were the pre-probe arithmetic
warnings; section 8 confirms the candidate's slot allocation, not a Graph-level storage regression.
Neither cost is accepted. Zero new adjacency bytes does not mean zero property-store overhead.

Two independent body arenas imply two additional regions if this candidate is adopted. No numeric
MemoryIds are reserved here. Under the existing ordinary four-page default policy, the first backing
allocation in each would round to 256 KiB (512 KiB for two); this is conditional arithmetic, not a
measured cost or a selected body-region policy. Existing property directories use a different,
64-page policy, so do not model their physical cost as the small-region default.

Measure at equal keys/payloads/operation order:

| Case | Required observation |
| --- | --- |
| Tiny get, missing key, key-only enumeration; neighbor 16 B→1 MiB | No external body reads; neighbor size must not produce growing directory overflow work once both cases are external. Separate the threshold crossing from large→larger scaling. |
| Tiny replacement beside a large value | No read/write of the unselected external body; report directory rewrite cost. |
| Small-only scalar/short-text-like encoded bytes | Directory density, read/write calls/bytes and physical growth, not only large-body wins. |
| T−1/T/T+1 and P−1/P/P+1 bodies | Classification, exact data, reference/header/tail cost. |
| Large read/replacement/delete and grow/shrink churn | Selected-body scaling, old+new high-water, freed-block reuse after reopen. |
| Rekey then source recreation and destination removal | Old source absent; destination exact; no premature free, alias or double-free. |
| Limit one below/exact body size; poisoned body memory | Rejection before body reads/allocation and before writes; exact allowance still encounters the real storage error. |

The first probe has two key widths, three thresholds and four block sizes, at most 256 entries and
8 MiB of encoded live input per fixture, with no individual body above 1 MiB. It measures native
Memory-interface operations, not Wasm instructions. Do not add repetitive PocketIC bootstraps.
Before production adoption, measure real Value decode, ordinary Graph paths and scoped canbench;
report regressions explicitly and investigate them. No acceptable-overhead percentage is assumed.

## 7. Activation and next decision

If adopted, record an ADR and update ADR 0007, the stable-layout registry/inventory and actual
PropertyStore contracts together. Require fresh Graph state or reinstall; do not migrate in place,
reinterpret the old envelope or add compatibility decoders. New-format reopen/upgrade must preserve
both directory and allocator state. Other canisters do not acquire new storage solely for this
change, but a wiped Graph must not be reattached to retained Router mutations, old vertex bindings
or stale derived-index state. Activation needs a coherent fresh logical-graph deployment/reset;
Graph-only reinstall is not assumed safe. Its exact reset set and deployment/wire compatibility
must be specified before integration, not left to an operator to infer.

The isolated probe is complete with the disposition below. Further allocator/write-path design
needs a separate bounded plan; this document does not select a production threshold or block size.
Bulk edge SET remains unavailable until bounded Graph selection, policy, whole-row preflight,
retained outcomes and Router integration are implemented separately.

## 8. Probe results and disposition

[Diagnostic source](artifacts/2026-09-13-property-value-separation-probe.rs), exact
`ic-stable-structures = 0.7.2`. Two native tests passed over 24 configurations and emitted 1272
measurement rows. The baseline includes the implemented store's one-byte envelope, but neither
implementation decodes Graph Values. Counts below are native Memory-interface operations, not
Wasm instructions, latency or IC syscall measurements.

### Neighbor isolation works

Representative case: 14-byte key, T=64, Q=256, two entries. The selected value stays ten bytes.

| Neighbor body bytes | Existing tiny-get calls / bytes | Candidate directory calls / bytes | Candidate arena reads |
| ---: | ---: | ---: | ---: |
| 16 | 7 / 62 | 7 / 63 | 0 |
| 4096 | 11 / 106 | 7 / 63 | 0 |
| 65536 | 71 / 766 | 7 / 63 | 0 |
| 1048576 | 1042 / 11447 | 7 / 63 | 0 |

At the 1 MiB neighbor, replacing the tiny value reads **1060024 B** and writes **1048647 B** in
the baseline. The candidate reads **140 B**, writes **85 B**, and never accesses the arena for
that operation. Missing/key-only lookups likewise perform no arena reads. All measured read operations
assert zero writes/grows. Exact returned bytes and keys are checked outside measurement.

### Common-path cost must not be hidden

For 256 ten-byte values with 14-byte keys (T=64, Q=256):

| Operation | Existing read calls / bytes | Candidate read calls / bytes | Existing / candidate write bytes |
| --- | ---: | ---: | ---: |
| Get all individually | 10026 / 110365 | 10026 / 110621 | 0 / 0 |
| Replace all, standalone setter | 11106 / 122245 | 21132 / 234202 | 46648 / 47984 |
| Replace all, matched previous-value-read control | 21132 / 232610 | 21132 / 234202 | 46648 / 47984 |

The standalone setter difference is real: this prototype gets the old cell before `insert` so it
can reject combined old/new bounds before writing. The matched control gives the baseline an
explicit prior `get`; it does **not** replace the original comparison or make the baseline bounded.
With that control, read bytes increase about 0.68% and write bytes about 2.86%, with equal read-call
counts. Graph already has a previous-value read in
[`commit_vertex_property_writes_bulk`](../../crates/graph/src/facade/store/properties.rs), but this
experiment is not that Graph path. Integration must reuse preparation at the owning boundary rather
than silently stack another lookup; real Graph/Wasm measurements are still required.

The small-only directory occupied one 64 KiB native page at T=32/64 but two at T=128 in this
256-entry fixture. The baseline occupied one; the separate arena header occupied another page even
with no external values. This demonstrates constructor sizing/rounding effects, not a universal
2× directory-size ratio or the production memory-manager footprint.

### Fixed blocks have conflicting costs

For a 65-byte external body at T=64, measured active slots plus the 14-byte reference are
**142 / 270 / 1038 / 4110 bytes** for Q=128/256/1024/4096 respectively, versus the baseline's
66-byte envelope. These exclude B-tree/free-capacity/region overhead. The large-block short-body
waste is not accepted.

For a selected 1 MiB body, the baseline uses 2077 reads / 1060013 bytes. Candidate figures are:

| Q | Directory reads / bytes | Arena reads / bytes | Active arena slot bytes |
| ---: | ---: | ---: | ---: |
| 128 | 7 / 65 | 18727 / 1198448 | 1198464 |
| 256 | 7 / 65 | 8741 / 1118560 | 1118720 |
| 1024 | 7 / 65 | 2083 / 1065296 | 1065984 |
| 4096 | 7 / 65 | 517 / 1052768 | 1056768 |

Small blocks reduce tail waste but substantially increase selected-body calls; large blocks help
large reads but waste space for short external bodies. No throughput conclusion follows from these
native read counts alone.

Replacement-body preparation also has a high-water cost. For one hot large property at Q=256,
old+new warm-up minted **2237440 B** of slots and backed **2293760 B** of arena memory, plus a
65536-byte directory. The baseline ended at 1114112 B. Eight post-reopen grow/shrink replacements
reused that capacity with **zero further arena growth**, and the final small value left all arena
slots free. The free-list/live-owner audit found no leaks, aliases or double-frees. This is a
single-hot-value experiment, not a claim that an entire graph must double in size. Reuse does not
shrink physical memory; the old+new peak is a real cost of this candidate's replacement strategy.

### Contract coverage and limits

- Exact roundtrips, all four representation transitions, old-value returns, deletion, same-key
  no-op and ownership-preserving rekey with/without a displaced destination; source recreation and
  an unrelated maximum key remain independent. These are opaque key moves, not LARA dispatcher tests.
- Exact body-byte/block allowance, combined old+new admission, zero-write/grow error checks with
  pre-existing data, oversized claimed length before output-buffer request, malformed cell/header,
  arithmetic overflow, short/long/cyclic/invalid body chains and repeated free IDs.
- Normal reopen retains values and allocator state; incompatible version/Q/T and partial directory/
  arena pairs fail closed. The partial-pair test genuinely failed on the initial prototype, which
  initialized a missing arena, then passed after the owner-level guard was added. This was a defect
  in the new prototype, not a finding against the implemented Graph store.
- The global live/free partition audit runs only outside measurement. It is not an allocation or
  production reopen algorithm. Explicit body-buffer request counters are not total heap accounting.

There are 24 baseline/directory/arena triples and one additional negative-test pair. B-tree calls:
49 creations and 48 successful reopens. Arena calls: 25 creations, 24 successful reopens and three
rejected version/configuration reopen attempts. Two partial Store reopen attempts reject before
any collection initialization. No PocketIC constructor, install, canister call or IC rollback test.

**Disposition:** retain value separation as the design direction, but do not select the fixed-block/
replacement-body candidate, T or Q for production. These costs motivated the later extent allocation/
reuse experiments linked above; those are not changes to this fixed-block probe. The completed
native PropertyStore comparison retains common-path lookup, directory and Value/move costs. Do not
claim an instruction win at the real Graph preparation/commit boundary from these native results.

### Reproduce and validation

From the repository root, use an isolated manifest rather than a new workspace member:

```sh
repo="$PWD"
probe="$(mktemp -d /tmp/gleaph-value-separation-probe-XXXXXX)"
printf '%s\n' \
  '[package]' 'name = "gleaph-property-value-separation-probe"' 'version = "0.0.0"' \
  'edition = "2024"' 'publish = false' '[lib]' \
  "path = \"$repo/design/investigations/artifacts/2026-09-13-property-value-separation-probe.rs\"" \
  '[dependencies]' 'ic-stable-structures = "=0.7.2"' \
  '[profile.dev]' 'debug = "line-tables-only"' \
  '[profile.test]' 'debug = "line-tables-only"' > "$probe/Cargo.toml"
cargo test --offline --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --lib -- --nocapture
cargo clippy --offline --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --lib --tests -- -D warnings
rustfmt --edition 2024 --check design/investigations/artifacts/2026-09-13-property-value-separation-probe.rs
```

`MEASURE` rows carry `case_bytes`: neighbor size for the tiny/missing/key-only series (including
`tiny_replace`), selected-body
size for large gets, and new-value size for replacements/boundary/churn cases. The paired
`MEASURE_HEADER` defines all columns. Construction, diagnostics, audits and assertions are excluded
from operation counters. Full logs and extracted CSV for the verified run are under
`/tmp/gleaph-value-separation-probe-mukvyfg1/`; the source reproduces them without that directory.

Final native validation: **2 passed**, 1.47s build + 4.82s runtime. Scoped Clippy passed (0.37s),
artifact rustfmt check passed. This is a same-assistant experiment/review, not independent/security
approval. No production Rust, workspace dependency/lock or persisted format changed. Graph/LARA,
PocketIC, workspace tests and canbench were not run; they remain later integration gates.
