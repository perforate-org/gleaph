# PropertyStore comparison: library B-tree bodies versus custom extents

Status: Experimental — native and managed full-store IC comparisons validated; production unselected.
Design anchor: 2026-09-13 22:19:21 UTC +0000.
Native verification: 2026-09-14 01:10:06 UTC +0000; [results and limitations](2026-09-13-property-store-native-comparison.md).
Managed full-store verification:2026-09-17 22:24:02 UTC +0000; [IC results](2026-09-17-property-store-managed-comparison.md).
Repository anchor: `95d7910878a5ee8af95af878f2b0df952edd70dc`.

## 1. Question and checkpoint

Compare storing, replacing and reclaiming property bodies through the existing B-tree library with
using the custom contiguous-extent owner. This is **not** a comparison of LARA bins with the extent
free index. The custom extent still uses the existing B-tree for free-space lookup.

The [managed body-only comparison](2026-09-13-property-body-extent-canbench.md) measured91 cases but
excluded a persisted property directory, canonical Value encoding/decoding and full property-key
movement. It cannot establish total PropertyStore cost. The completed checkpoint is a **native,
Graph-owned test prototype** including those operations and all required memories. The separate
full-store IC comparison now measures the same operations; native counters alone do not decide speed.

Keep four-page allocation-policy assumptions unchanged; the proposed one-page index policy,
zero-fill optimization and selected-key lookup optimization are deferred. Reserved capacity is
reusable by its owning region, not dead bytes, but cannot be spent by another region. Report both
used bytes and retained backing rather than rejecting a design solely for an initial region floor.

## 2. Compared configurations

| Configuration | Stored property representation | Independently owned memories |
| --- | --- | --- |
| Direct reference | Existing property-key→StoredPropertyValue B-tree | 1 |
| B-tree4KiB | Bounded directory + property-keyed body chunks, P4096 | 2 |
| B-tree16KiB | Same, P16384 | 2 |
| B-tree64KiB | Same, P65536 | 2 |
| Custom extent | Same directory contract + contiguous bodies + free-space B-tree | 3 |

The direct reference uses the actual VertexPropertyStore/EdgePropertyStore methods and codec, not
an extra directory placed in front of them. It remains the **unbounded read-admission reference**:
those methods cannot reject an old body by length before fetching it. Successful logical behavior
is compared with the candidates, but unsupported pre-body admission is recorded as a capability
limitation, not hidden by adding a shadow length map or weakening the candidate tests.

Keep all three already-studied chunk caps. In particular, retaining16KiB avoids selecting only the
slower4KiB large-write control to favor custom extents. There is no new cap or threshold sweep.
Chunk values continue to use default B-tree node geometry with an independently enforced owner cap.
Each store instance owns its own memories; vertex and edge stores do not share a body allocator.

## 3. Reuse and execution boundary

The prototype is a test-only child of Graph's existing `facade::stable` module. This permits
reuse of the actual private Graph storage definitions without making them public or importing
Graph into portable GQL crates:

- [VertexPropertyKey, StoredPropertyValue, VertexPropertyStore](../../crates/graph/src/facade/stable/vertex_properties.rs);
- [EdgePropertyKey, EdgePropertyStore](../../crates/graph/src/facade/stable/edge_properties.rs);
- [ensure_property_id and ensure_persistable](../../crates/graph/src/property/persisted.rs).

Keys retain their actual8/14-byte Storable encoding and numeric/prefix ordering. Value bytes are
produced and decoded by StoredPropertyValue, including its V1 envelope and IC extension decoder.
Do not copy that envelope encoder, invent another Value codec, use serde as a substitute, or move
these private definitions into a new production utility crate just for the experiment.

Reuse the [B-tree body owner](artifacts/2026-09-13-property-body-btree-probe.rs) and
[extent owner](artifacts/2026-09-13-property-body-extent-probe.rs). Necessary changes are narrowly
limited to supplied-memory construction, canonical key parametrization, local prepare/publication
hooks and crate-private test access. Keep one algorithm per owner and preserve their six native
tests and260+36 I/O rows. Do not copy either allocator into the PropertyStore prototype.

The Graph hook compiles only under `cfg(all(test, not(target_arch = "wasm32")))`. No global Graph singleton,
Router endpoint, production storage dispatch, registered MemoryId or canister export changes.
The isolated IC harness source-includes the actual private Graph owners and this same prototype,
without changing the Graph hook. It constructs distinct managed IDs itself; native supplied-memory
alias rejection is not a public Wasm alias-validation API. Native-only fixtures remain under cfg(test).
A common test operation contract does not require a runtime strategy registry or public storage trait.
Small mechanical vertex/edge adapters should forward the existing key accessors and validation.

## 4. Byte convention and experimental directory

Let **S** be `StoredPropertyValue::V1(value).into_bytes().len()`: the entire canonical stored value,
not plain text, JSON or the earlier raw-body fixture's arbitrary bytes. The prototype limit is
S≤1MiB; it is an experimental fixture/API limit, not a newly selected product limit. Retain the body
owner's representability checks; do not claim support for every production-sized Value from this
finite corpus. Canonical encoding may allocate before S is known; this is not an input-decoder heap
admission proof.

Use one fixed experimental small threshold: **S≤65 bytes**. With the inspected V1 codec this includes
up to64 bytes of encoded Value, but classification uses S, not a copied codec-size formula. The body
stores retain the complete S bytes so all decoding uses the owning StoredPropertyValue implementation.

The directory uses the actual property key K and a private cell codec, with
`Storable::BOUND = Bounded { max_size:66, is_fixed_size:false }`. Cells are not padded to66 bytes.
The following formats are experimental and reject other markers; no legacy decoder is added:

| Cell | Encoding | Encoded size |
| --- | --- | ---: |
| Metadata | `0x30`, key width u8, body kind u8, threshold u16LE, chunk cap u32LE | 9 |
| Small | `0x31`, complete canonical stored-value bytes | 1+S, at most66 |
| External chunks | `0x32`, S as u32LE | 5 |
| External extent | `0x33`, header offset u64LE, S as u32LE | 13 |

Metadata body kinds are1 for chunks and2 for extents; chunk cap is4096/16384/65536 for kind1 and0
for kind2. It records key width8/14 and threshold65. The marker family owns this experimental format.
The single metadata row uses the key constructed with all fields zero through the real key
constructor. PropertyId0 is already reserved by Graph, so no legal property collides with it.
Public-like reads/removes of property0 return absence and writes use the existing reserved-ID error;
entity enumeration must never expose the metadata row. No other reserved-ID rows are permitted.

This uses the existing directory collection to own its immutable schema, rather than adding a
metadata MemoryId or a BTreeMap<(),Metadata>. Count this one extra row, its key/value bytes and any
packing/page consequences. The direct reference has no such row. Metadata/cell compatibility checks
belong to the candidate store, not the Value codec or body allocator.

An external body must have S>65, a representable length and the cell kind matching the store's
metadata. The directory is the sole durable source of its encoded length. The extent's tags remain
canonical for physical capacity/state; its free index remains derived.

### Body key and reference ownership

B-tree chunks use `(K, ordinal:u32BE)`, giving **12-byte vertex /18-byte edge body keys**. Reuse K's
Storable implementation; do not pad vertex prefixes to14 bytes or copy the property-key field codec.
The existing opaque14-byte raw-body prefix remains the default specialization for old fixtures.
No body-ID counter, global reference registry or length map is needed: K implicitly identifies the
chunk prefix, so the external cell stores only S. This makes key movement more expensive and that
cost must be measured, not hidden behind an unmeasured indirection layer.

Extent cells carry reusable physical offsets. A move can transfer that reference without rewriting
source payload bytes. It still reads source Value when the operation must return it, and must retire
any displaced target exactly once. Offsets are never lifetime edge IDs or cross-message handles.

### Creation and reopen

For separated candidates, before initialization require all supplied memories to be distinct and
either all empty or all initialized. Refuse partial sets and aliases before effects. Creation uses owning `new` operations
and writes the directory metadata; any unexpected failure after the first effect traps/panics.
Reopen uses owning `load`, then checks directory metadata before opening body storage. Never recreate
missing metadata or a missing body partner with `init`, scan all bodies to rebuild ownership, or
reinterpret another format. Metadata field mismatches in the same key type return a layout error;
arbitrarily malformed upstream bytes or a wrong concrete key decoder may trap, without claiming a
universal typed corruption API. Reopen over valid state must make no writes or grows.

## 5. Logical operations and admission

| Operation | Required behavior |
| --- | --- |
| get(K) | Absent→None; stored NULL→Some(Value::Null); otherwise the exact decoded Value. |
| set(K,V) | Replace the complete value, returning the previous Option<Value>. All small/external transitions are supported. |
| remove(K) | Return the previous Option<Value>; absent is a no-op; release only the removed property's body. |
| enumerate(scope) | Exact property-ID order and scope, including maximum IDs; complete private result or a candidate limit error, never a silently truncated success. |
| rekey(src,dst) | Single-property move, same property ID; return the moved source Value if moved. Same key or absent source is a no-op. Replace a matching destination property and preserve every unrelated entry. |

Edge rekeys keep owner/label/property fixed and change slot. Vertex-key moves are a synthetic
key-width/control case, not a new public vertex-move operation. The direct rekey adapter composes
existing get/remove/set after validating the moved input before its first write; report that adapter,
not a nonexistent production single-property API. This is **not** proof of whole-edge `move_all_for_edge`
or an atomic multi-property batch. Those require an additional interacting-plan contract.

Candidate operations have one selected encoded-byte allowance, initially2MiB in success fixtures:
get/remove charge old S; set charges old S+new S; rekey charges source S+existing destination S.
Rekey reuses acquired source bytes rather than charging them again as a fresh caller input.
Enumeration charges the sum of selected S and has a maximum output-property count, initially256.
These are selected payload/input/output admissions, not total I/O, heap or instruction allowances.
Small directory cells can be fetched before that charge but are capped by the directory codec.
External lengths are admitted **before body metadata/payload access or body-buffer allocation**.
No external body access occurs for small-only get or missing-key lookup.

Use codec-roundtrippable fixture Values; an encodability check alone does not prove decodability of
arbitrarily deep composites or unknown extensions. Do not change the production validator here.
Use canonical equality for ordinary Values; for NaNs or similar non-reflexive values compare the
owning codec's round-tripped bytes/classification rather than weakening the whole oracle. Cover
NULL, empty/Unicode text, scalars, a bounded composite and an IC extension. Generated long text must
have position-distinguishable chunks; changing payload content must not be described as changing
immutable Value type/length/version bytes. No unchanged-chunk write elision is introduced.

The direct API does not accept these allowances. Do not insert costly validation lookups into its
ordinary get/set/remove timings and call that the original baseline. Run its actual successful
operations, reserved-ID/Value validation and lifecycle contracts; identify the candidate admission
contract it cannot satisfy separately.

## 6. Local preparation and directory publication

All candidate mutations run under one exclusive synchronous owner access. Complete new-value
validation/encoding, required source/destination acquisition/decoding, byte admission, cell checks,
body geometry and retirement preparation **before the first write or grow**. Prepared data is private,
binds the exact new bytes, and cannot survive an intervening mutation, reader or await.

The body owners need an opaque local preparation followed by infallible publication/commit, not
calling the existing setter and then hoping a later directory error rolls back:

- Extent separates its existing prepare logic from commit and lets the directory publish at the
  existing descriptor-publication point. New payload precedes the new directory reference; old
  body retirement follows it. The raw-body wrapper still publishes its caller Option and keeps its
  original behavior/counters.
- Chunk replacement acquires old bytes once, prepares the exact new/obsolete key ranges, then
  writes the new chunks and publishes the directory cell before retiring obsolete chunks. A new
  explicit create preparation handles absent prefixes; do not present fixture `seed` as a production
  replacement API. No recoverable error after its first insert/remove/directory write.
- Small→external writes the new body before publishing its cell; external→small publishes the
  already-encoded small cell before retiring the old body. Small→small stays in the directory.
- Chunk rekey rewrites source chunks under the destination prefix; extent rekey transfers the
  source reference. Both preflight source/target, publish destination and remove source directory
  ownership, then retire obsolete/source or displaced storage as applicable. A destination with
  no previous entry is an explicit state, not a fabricated zero-length body.

A bounded infallible publication closure inside the existing committing owner is sufficient; no
transaction framework, generic memory-write overlay, shadow journal or rollback-by-cleanup.
Unexpected post-fence failures panic/trap. Native tests cannot prove IC rollback. The baseline rekey
adapter must likewise never propagate an ordinary error after its first destructive operation.

Directory B-tree mutation itself can allocate, read and rewrite nodes; count it. Internal commit I/O
is not an intervening external read. Individually prepared operations do not establish whole-row
allocation safety, remap validity, index/outbox bounds or durable positive/zero/rejected outcomes.

**Experimental store seam implemented; Graph integration still planned:** the
[prepared-change experiment](2026-09-18-property-store-prepared-change.md) implements `with_prepared`
in the shared candidate. It lends an exclusive exact-input capability to a synchronous caller, exposes
the previous Value, and either drops without effects or consumes commit once. Existing body owners
and stored formats are unchanged. The [Graph source study](2026-09-17-property-store-previous-value-reuse.md)
explains the intended consumer: its existing index planning, not an additional facade get. A decoded
caller Value alone is not preparation evidence. Actual Graph integration must finish all fallible
body/cell/admission and index planning before the durable index-build reservation/outbox fence.
That integration, whole-row plan composition and production activation remain unimplemented.

## 7. Native matrix and measurement boundaries

Use five configurations × two real key types × four fixture families = **40 full-store creations**.
The four new test functions each loop over the same ten configurations; no global shared live store.

1. **Scalar/scope/density:**256 small properties distributed over several scopes, exact get/order,
   replacement, alternating128 removals, valid reopen/refill; then65/66 stored-byte threshold cases.
   Include owner0 (metadata exclusion), maximum IDs, independent slot-/label-/owner-only edge
   scope foils and NULL versus absence.
2. **Large/neighbor/reuse:** cold small→1MiB before large prewarming; full replacement and shrink;
   selected65-byte small and66-byte external values beside1MiB;64KiB cases; reopen then four grow/
   shrink pairs, then an S66 source/S97 live blocker forcing1MiB relocation and final removal.
   Assert the new address starts at the former arena end and the old source is retired; retain exact
   guard bytes and complete ownership. Same-key/missing-source moves preserve physical bytes.
   Freeing never shrinks backing; B-tree deletion may allocate while updating nodes. Do not require
   no growth on every library deletion or hide such growth. Extent retirement retains backing.
3. **Fragmentation/rekey:** start cold with128 alternating external S66 bodies and S97 guards
   (256 properties); remove the S66 bodies. Their88-byte holes cannot hold another S97 body, which
   requires120 bytes. Measure no-fit insertion, then remove the first guard outside measurement
   and insert S257 into the coalesced296-byte span (280 required,16 absorbed). Audit exact placement
   and all surviving guards. Clear logical properties, then run the warmed missing/self and
   Small/External source × absent/Small/External target rekeys, including distinct overwritten
   values, source recreation, destination deletion and post-reopen reuse. Use a1MiB external source
   and target case. Metadata-only reference transfer does not excuse returned source acquisition.
4. **Rejection/reopen:** reserved IDs/a small extension without a binary encoder (the existing
   InvalidExtensionType contract), exact and one-short candidate allowances,
   over1MiB inputs, corrupt selected cell/body, missing/extra/wrong-width chunks, invalid extent
   geometry, alias/partial memories and metadata mismatch. Snapshot every affected memory and all
   actual directory entries; assert precise errors and zero effects, not only an error result.
   Poison source and displaced-target Value envelopes separately to check both preflight paths;
   test edge cross-owner/label and cross-property move rejection.

Each fixture uses at most256 live properties,1MiB per encoded stored value and8MiB live payload.
Temporary replacement capacity and byte/decoded-value buffers may exceed live payload; report them
separately. Fixture families can have several measured operations but cannot introduce hidden stores.

The40 constructions imply72 B-trees and8 raw arenas: per key/fixture group, Direct1, each chunk
candidate2, extent2 B-trees plus its arena. Require one successful full-store reopen per fixture
(40, normally72 B-tree loads); count rejected reopen attempts separately. Use at most six same-key-type
metadata failure attempts for each of the eight candidate rejection fixtures (48 directory loads);
alias/partial sets reject before collection load. Additional constructors require plan revision.

Including the reused six raw-body tests gives ten test functions,108 B-tree creations and17 arenas
per full Graph probe suite; old raw fixtures retain their36 creations/30 successful loads. Keep
these raw and full-store counters/results separate rather than concatenating unlike byte axes.

Native uses counted VectorMemory for each region, as the owning probes do. Record per-region calls/
bytes/grows and sum raw backing pages; include metadata rows, directories, chunks, free indexes,
reuse/high-water and relocation peak. The four-page manager projection may be shown separately as
`2 + Σ 4*ceil(region_requested_pages/4)`, explicitly **not native physical-manager measurement**.
Actual managed instructions/capacity are recorded separately in the IC report. This projection is for an isolated
store fixture; do not count shared manager metadata once per store when estimating a whole Graph.
Graph's production64-page property policies are not substituted for this experimental projection.

I/O closures include input validation/encoding, owning store operations and returned Value decoding;
setup, generation, exact-state audits, reporting and returned-value destruction stay outside.
Memory counters do not measure codec CPU/heap, so this checkpoint cannot decide the speed winner.
Use cold and warmed histories explicitly and compare all owners on the same logical operation order.

## 8. Acceptance and later decisions

Accept this checkpoint only with exact logical/property-byte outcomes, directory-to-body reachability,
free-index/partition equality, no alias/leak after moves and successful preservation of old raw
contracts. Whole-reference limitations must remain visible. No hard total-work guarantee follows
from a chunk cap or an encoded-byte budget; tree height/save/rebalance, allocator work, Value decoder
workspace and memory-manager reopen still need numerical production admission contracts.

The completed IC comparison uses the same full-store implementation, retains actual direct point-get,
measures both scalar-dominated and large/mutation-heavy cases, and includes every region. No traffic-weighted
winner can be selected from static seeds. Only after that comparison should a production ADR choose
layout, threshold, activation and Graph integration. Require coherent fresh logical state rather than
legacy decoding or a Graph-only reset attached to retained Router/index state.

Encapsulation and invariants stay with the existing PropertyStore/body owners; canonical lengths
are not duplicated, derived state has one committing writer, and portable GQL/Router/index/adjacency
boundaries remain unchanged. The extra metadata row, cell forms and prepare/publication seam are
necessary to measure a persisted directory safely; they are not a new production subsystem.

The native implementation and measured scope are recorded in the [native report](2026-09-13-property-store-native-comparison.md).
The prototype is not production storage. The separate IC report validates experimental query-local
costs, not production adoption, update/upgrade lifecycle, independent/security approval or bulk edge
SET availability.
