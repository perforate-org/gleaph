# 0067. Stable linear hash map production contract

Date: 2026-08-12
Status: Partially Implemented
Last revised: 2026-08-14
Anchor timestamp: 2026-08-14 08:35:42 UTC +0000

> **Partially implemented contract.** This ADR is the sole exact V1 persisted-format contract for
> ic-stable-linear-hash-map. The map and the Vector MemoryId 4 and 7 owner cutovers are implemented;
> a production-authorized coordinated reset remains pending.

## Context

ic-stable-linear-hash-map owns one stable-memory region for fixed-width point-map operations. Vector
uses it through two private owners: `DEFINITION_STORE` for Router-assigned index ids at MemoryId 4
and `SUBJECT_STORE` for `SubjectKey -> FixedSubjectMapEntry` at MemoryId 7. Router assigns
identifiers and authorizes work; Vector owns the bindings and domain outcomes; the generic map owns
persisted bytes and their invariants.

ADR 0007 remains authoritative for the stable-region inventory. ADR 0064 permits a destructive
pre-release Vector reinstall. This ADR permits neither an alternate reader nor conversion of a
nonempty region.

## Problem

The map needs one inspectable byte contract, fixed-cost normal reopening, an explicit destructive
reset, a bounded full-integrity operation, and resumable bounded physical enumeration for owners
without an external key catalog. Width-only checks are unsafe because equal widths do not establish
compatible key storage, key routing, or value storage. Scanning every allocated bucket on open makes
upgrade cost grow with table history.

## Existing architecture assessment

The existing MemoryManager/VirtualMemory boundary is sufficient. A new registry, manager, or
Router-owned storage facade would duplicate state ownership. The map crate alone owns layout,
schema validation, address arithmetic, routing, split geometry, mutation fencing, reset, physical
scan, and scrub.
The Vector facade owns the MemoryId 4 and 7 staged availability and map-error translation boundaries.
Graph and Router never read or write raw map bytes.

## Alternatives

### A. Retain a second accepted byte interpretation

Rejected. This is a destructive pre-release format decision. A second reader, mode, or conversion
path would preserve an unreviewed second contract.

### B. Persist offsets, strides, slots, or geometry derivatives

Rejected. They are derivable from V1 constants and key/value widths. Persisting them duplicates
knowledge and permits disagreeing values.

### C. Scan buckets during ordinary open

Rejected. It makes normal reopen proportional to table size. Complete validation belongs to an
explicit bounded scrub.

### D. Add a journal or resumable split state now

Rejected. One permitted split is bounded and completes in one update. Persisted recovery state is
not justified until a demonstrated multi-call mutation needs it.

### E. Expose an ordinary iterator or maintain an external key catalog

Rejected. An iterator cannot express upgrade-stable progress or a bounded instruction contract, and
an external catalog duplicates canonical keys plus their write-consistency obligation. The owning
map can enumerate its fixed physical slots directly under a per-step epoch fence.

## Decision

### 1. One destructive pre-release V1 format

The only supported layout has magic LHM and version exactly 1. V1 is destructive and
fresh-memory-only: strict create accepts only memory.size() == 0; any nonempty region must pass
exact V1 open validation or is rejected without writes. There is no alternate reader, conversion,
fallback create, or reset-on-open-error behavior.

All integers are unsigned little-endian. Every range still designated reserved is written and
validated as zero. This ADR revision explicitly assigns the pre-release control bytes 160..168 to
the backward-relocation generation; it is not an implicit reinterpretation by a reader.

### 2. Exact immutable 128-byte header

The immutable header occupies bytes 0..128. It persists only schema identity, the fixed key/value
widths, and the immutable hash seed; it contains no derivable layout or geometry fields.

| Bytes | Type | Field | Required value |
|---:|---|---|---|
| 0..3 | [u8; 3] | magic | ASCII LHM |
| 3 | u8 | layout_version | exactly 1 |
| 4..8 | u32 | key_size | fixed key storage width |
| 8..12 | u32 | value_size | fixed value storage width |
| 12..16 | [u8; 4] | reserved | all zero |
| 16..32 | [u8; 16] | key_storage_schema_id | type-owned key storage identity |
| 32..48 | [u8; 16] | key_routing_schema_id | type-owned routing identity |
| 48..64 | [u8; 16] | value_storage_schema_id | type-owned value storage identity |
| 64..72 | u64 | hash_seed | immutable seed selected at creation |
| 72..128 | [u8; 56] | reserved | all zero |

StableHashKey owns KEY_STORAGE_ID and KEY_ROUTING_ID; StableMapValue owns VALUE_STORAGE_ID. These
are frozen nominal identifiers, not hashes of type names, compiler layout, Rust layout, or Candid
text. Constructors accept no caller-supplied identifier. Matching widths without matching
identifiers, or matching identifiers without matching fixed widths, is invalid.

V1 key routing is frozen as part of `key_routing_schema_id`: RapidHash V3 exact mode hashes the
canonical `StableHashKey::stable_hash_bytes()` output with secrets seeded by
`hash_seed ^ 0x1319_8a2e_0370_7344` for candidate 0 and
`hash_seed ^ 0xa409_3822_299f_31d0` for candidate 1, in that order. The immutable header seed is
the only seed input and is never replaced within an incarnation. For level `l`, reduction first
uses the low `l` hash bits; when that bucket is below `split_cursor`, it instead uses the low
`l + 1` bits. Bit-mask reduction and modulo by the corresponding power of two are required to be
equivalent. The implementation's literal route and reopen-byte vectors are V1 conformance tests;
changing the hash version, either domain constant, candidate order, reduction rule, canonical key
bytes, or seed semantics requires a new explicit format decision and must not exact-open as V1.

### 3. Derived V1 layout and exact mutable control

V1 fixes HEADER_BYTES = 128, CONTROL_BYTES = 64, BUCKET_SLOTS = 8, and
BUCKET_HEADER_BYTES = 8. The fixed mutable control follows the immutable header at bytes 128..192;
buckets begin at byte 192.

| Bytes | Type | Field | Invariant |
|---:|---|---|---|
| 128..136 | u64 | len | len <= physical_buckets * 8 |
| 136..144 | u64 | physical_buckets | at least 8; sole persisted geometry scalar |
| 144..152 | u64 | mutation_epoch | committed states are even; writing state is odd |
| 152..160 | u64 | incarnation | starts at 1; increments exactly once per reset |
| 160..168 | u64 | backward_relocation_generation | starts at 0; advances on each committed backward one-hop resident move |
| 168..192 | [u8; 24] | reserved | all zero |

Within the 64-byte control record these are offsets 32..40 for
backward_relocation_generation and 40..64 for the remaining reserved-zero bytes.

The header is immutable for its lineage. The control is the sole mutable map-level record. Seed,
layout offsets, sizes, bucket slots, slab offsets, page stride, level, and split cursor are never
persisted in control.

Every address and extent is recomputed with checked arithmetic from V1 constants and the header
widths:

    key_slab_offset     = BUCKET_HEADER_BYTES
    value_slab_offset   = BUCKET_HEADER_BYTES + BUCKET_SLOTS * key_size
    bucket_page_stride  = value_slab_offset + BUCKET_SLOTS * value_size
    bucket_base(i)      = 192 + i * bucket_page_stride
    required_end        = 192 + physical_buckets * bucket_page_stride
    capacity_entries    = physical_buckets * BUCKET_SLOTS

Each bucket has an eight-byte little-endian occupancy header; only its low eight bits are occupied
slot bits. A slot's key and value share the same slot index. For a valid physical_buckets = n,
derive level = floor(log2(n)), base = 1 << level, and split_cursor = n - base; require level >= 3
and base <= n < 2 * base.

### 4. Strict create and ordinary O(1) open

Strict create does not inspect or overwrite nonempty memory. On zero-sized memory it validates all
schema and arithmetic, calculates the initial eight-bucket extent, and grows exactly once. Stable
memory zero-fill establishes empty bucket pages, so strict create performs no bucket-clearing loop.
It writes the initial control snapshot first and then writes the immutable header with magic as the
final publication. Initial control is len = 0, physical_buckets = 8, mutation_epoch = 0,
incarnation = 1, and backward_relocation_generation = 0; the header receives the trusted creation
seed.

Canonical open-or-create invokes strict create only at memory.size() == 0. Otherwise it exact-opens;
an open error is final and never falls back to create or reset.

Nonempty open is O(1): it reads and validates exactly the 128-byte header, the 64-byte control, and
the memory extent. It validates magic, V1, widths, schema identities, all reserved-zero bytes, an
even epoch, nonzero incarnation, derived geometry, and the checked allocated extent. It reads zero
bucket headers, keys, or values and does not reconcile occupancy with length.

### 5. Reset and mutation failure boundaries

Reset is an owner-only destructive operation on an already valid, opened V1 map. It takes
expected_incarnation; a mismatch returns a typed error that includes the current incarnation before
writes. Before writing, it preflights successor incarnation, exhaustion, the initial extent, and any
required one-time grow. If the current incarnation is u64::MAX, it returns a typed exhaustion error
with exact bytes unchanged. The immutable header, including its hash seed, is never rewritten by
reset.

On success reset writes an odd mutation epoch first. It clears exactly the eight initial bucket
occupancy headers and writes the new mutable initial state. It never scans or shrinks trailing
pages, resets backward_relocation_generation to zero under the successor incarnation, and publishes
the final even control snapshot last. Resetting the generation is safe because the incarnation
change independently invalidates every earlier cursor and makes generation zero a new lineage.

This map-local reset operation is implemented in the V1 collection crate. Vector currently uses
the coordinated definition-domain reset only in `cfg(test)` / `cfg(feature = "canbench")` fixtures:
an internal owner-issued ticket follows exact-incarnation/epoch preflight, all coupled region
handles are acquired before the first write, and the complete definition-dependent domain resets in
one fixture operation. A production-authorized coordinated reset remains pending, and no public
reset API is exposed.

At the owning facade, a successful reset and every prewrite reset error, including expected-
incarnation mismatch and exhaustion, keep Ready -> Ready. Unavailable is reserved for an exact-open
or integrity failure that makes the already-owned region unsafe to serve; it is not a reset-result
classification. If the caller cannot distinguish whether a reset request reached a successful IC
update boundary, it must exact-open and inspect the resulting incarnation: successor means success,
the expected current incarnation means no reset committed, and any other valid incarnation is an
ambiguous concurrent-owner outcome that must not be replayed blindly.

Every ordinary mutation finishes serialization, routing, bounded admission, checked arithmetic,
any required single grow, and its complete write plan before the first stable-memory write. A
returned prewrite error preserves exact bytes and logical state. After the first logical write, an
unexpected generic-memory failure leaves an odd epoch and fails closed; at the IC update boundary it
traps so the platform rolls back coupled stable writes. A live page is exactly a page with
i < physical_buckets. When a split reuses an already allocated page, it overwrites that complete
final page before publishing the larger physical_buckets control value.

An absent insert may perform at most one next-in-order bounded split. Below the split threshold, a
full current candidate pair is admitted only by a deterministic one-hop relocation: candidate
buckets are scanned in candidate order and occupied slots in ascending order (at most sixteen
residents); a resident may move only to its own other candidate. At or above the threshold, the map
first plans the normal next-in-order split. If its prospective target pair remains full, the map
keeps the current geometry and retries current-geometry admission, including that same bounded
one-hop relocation, before returning pressure. All resident decoding, routing, checked offsets,
complete source/destination page images, and the observed epoch are preflighted before the odd
epoch or any write. TablePressure therefore means neither the prospective split nor the bounded
current-geometry admission can place the key; it preserves bytes, control, and geometry exactly.
It is a typed terminal admission outcome, not allocation failure or a generic retry.

The map computes the source and destination physical slot for every planned one-hop resident move.
Only a destination below the source advances backward_relocation_generation, regardless of load,
capacity, or whether one-hop admission followed a rejected prospective split. Its successor uses
checked addition during planning; u64::MAX returns
`MutationError::RelocationGenerationExhausted` before any stable write. On apply, the odd epoch is
published first, the successor generation is persisted before either resident page, and the final
even epoch remains the last publication. Direct insert, overwrite, remove, split redistribution,
and a forward one-hop resident move do not change the generation.

### 6. Explicit bounded scrub

scrub_step is an administrative integrity operation, never initialization or public iteration. Its
captured map snapshot contains exactly the schema identities, hash seed, even mutation epoch,
incarnation, len, and physical_buckets. Geometry is always derived. Cursor progress is a
handle-session next primary-bucket position and accumulated occupied count, not persisted map state
or a wire encoding. It is replayable only on the open handle that created it. Reopen, upgrade, or a
separate alias starts a new scrub session at bucket zero.

Each call takes a positive primary-bucket budget and may perform only bounded candidate-bucket probes
needed to validate routing and duplicate placement. It validates cursor bounds and the exact captured
header/control fence before scanning, examines no more than the budgeted primary buckets, and
rechecks the same fence at completion. Retrying the same cursor replays the exact same bounded work.
Any mutation changes epoch; reset changes incarnation; either makes the cursor stale. Fixed-width
bytes that decode and then re-encode noncanonically return a typed integrity error. This contract
does not make user-defined `Storable` decode/encode, routing-byte, or equality callbacks fallible:
their panic traps under wasm panic-abort and fails closed at the IC update boundary. Host tests may
catch such panics as a diagnostic convenience but do not establish a typed wasm error.

Completion requires the captured primary-bucket bound, complete occupancy/routing validation, and an
occupied count equal to captured len. Cursor state is handle-local and noncanonical; abandoning it
and restarting from bucket zero is safe and required after reopening or upgrading the map.

### 7. Serializable bounded physical scan

`scan_start` returns a serializable cursor at physical slot zero. Its current encoding is exactly 96
bytes; all integer fields are unsigned little-endian:

| Bytes | Type | Meaning |
|---:|---|---|
| 0..3 | [u8; 3] | magic `LHS` |
| 3 | u8 | cursor version, exactly 2 |
| 4..8 | [u8; 4] | reserved, all zero |
| 8..24 | [u8; 16] | key_storage_schema_id |
| 24..40 | [u8; 16] | key_routing_schema_id |
| 40..56 | [u8; 16] | value_storage_schema_id |
| 56..64 | u64 | immutable hash_seed |
| 64..72 | u64 | incarnation |
| 72..80 | u64 | physical_buckets |
| 80..88 | u64 | next_slot |
| 88..96 | u64 | backward_relocation_generation |

The cursor persists no mutation epoch, length, level, split cursor, or handle lineage. It survives
serialization, exact reopen, and canister upgrade while schema identities, seed, incarnation, and
physical_buckets and backward_relocation_generation still match. An exact 88-byte version-1 cursor
is structurally decoded as legacy/stale, and `scan_step` returns `RestartRequired` before reading a
slot. No generation is inferred for it. Unknown size/version pairs, a zero incarnation, fewer than
eight physical buckets, overflowing slot capacity, next_slot beyond capacity, bad magic, or nonzero
reserved byte are malformed. A cursor with a different immutable schema or seed does not belong to
the map.

`scan_step(cursor, physical_slot_budget)` requires a positive budget and examines the half-open
physical-slot range from next_slot through `min(next_slot + budget, physical_buckets * 8)`. It
returns decoded key/value entries in physical order, the advanced cursor, the exact examined-slot
count, and explicit exhausted state. A short entry page is not EOF; only exhausted is EOF. Calling
again with an already exhausted cursor returns no entries, zero examined slots, and exhausted.
Because key/value encodings are fixed-width, the physical-slot budget also bounds returned entries
and payload bytes without a second output parameter.

Each step reads and requires one even mutation epoch before examining slots, collects output only
locally, then reads the complete control record again. A changed or odd epoch with unchanged
incarnation, geometry, and backward-relocation generation returns `InProgress`; a changed
incarnation, physical_buckets, or backward-relocation generation returns `RestartRequired`. Both
discard the local output. A malformed cursor, exact legacy cursor, or zero budget is rejected before
slot reads. Mutations between successful steps are permitted when the restart fields remain
unchanged: direct insert, overwrite, remove, and forward one-hop relocation do not fence an entire
multi-call lap. A committed backward one-hop relocation increments the generation exactly once and
forces restart so an unvisited resident cannot move behind next_slot unnoticed. Therefore an
unchanged map is enumerated exactly once in physical order and a repeated cursor replays the same
page, while a lap that overlaps permitted mutations has no cross-step snapshot guarantee.

Physical scan is not scrub. It does not validate routing reachability, duplicates, canonical
re-encoding, or captured length, and it never reuses the handle-bound `ScrubCursor`. The crate
exposes no ordinary `Iterator` and maintains no external key catalog.

### 8. Vector and Graph boundary

The Vector facade binds `DEFINITION_STORE` to MemoryId 4 and `SUBJECT_STORE` to MemoryId 7. Each
owner has the private lifecycle `Uninitialized -> Ready | Unavailable(reason)`; static construction
does not open memory. Fresh install strict-creates with its own trusted seed (`definition_map_seed`
or `subject_map_seed`), and post-upgrade seed-free exact-opens the existing region. Unavailable and
uninitialized reads/writes return typed unavailable results and never infer empty state. Existing
CHM or unknown bytes are rejected unchanged and require a destructive pre-release wipe/reinstall;
there is no reader, migration, or create-on-open-failure fallback.

`SubjectStore` alone obtains the MemoryId 7 handle and owns all subject point access plus physical
scans. Its durable `SubjectScanCursor` envelope contains a version, a consumer scope, and the exact
serialized LHM `ScanCursor`; it validates the envelope before any physical slot read. Detach and
rebuild Sampling, Building, Cleaning, and Aborting use positive-budget physical pages, explicit EOF,
and restart after an LHM split, reset, backward relocation, or legacy-cursor decode invalidates the
saved cursor.

Vector maps definition and subject admission pressure to distinct terminal errors and map
availability failure to distinct outer errors. It preflights each definition, then admits a new
subject before any coupled row, page, tombstone, or deleted-list write. Its batch outcome
distinguishes a committed prefix from one terminal item:

    VectorSyncBatchOutcome =
      Progress { applied: u32 }
      | Terminal {
          applied: u32,
          failed_index: u32,
          error: IndexDefinitionTablePressure | SubjectTablePressure,
        }

Terminal requires failed_index == applied < operations.len(). Graph removes exactly the
acknowledged prefix and quarantines exactly the failed item in one update, or changes neither.
Transport, decode, malformed reply, and unavailable outcomes keep the submitted outbox slice
unchanged.

**Implementation status (2026-08-14):** MemoryId 4 uses
`StableLinearHashMap<u32, VectorIndexDef>` and retains the fixed 41-byte definition schema;
MemoryId 7 uses `StableLinearHashMap<SubjectKey, FixedSubjectMapEntry>` and remains the canonical
subject freshness, deletion, and slot source of truth. Both owners start inert, strictly create on
install, and exact-open seed-free after upgrade. Failed exact opens remain unavailable and never
create, reset, or infer an empty map. The former broad `init_from_args` clearing path is not an
authorized production reset.

`gleaph-graph-kernel` defines the additive shared `VectorSyncBatchOutcome`,
`VectorSyncTerminalError::{IndexDefinitionTablePressure, SubjectTablePressure}`, and
`VectorSyncBatchUnavailable::{IndexDefinitionStoreUnavailable, SubjectStoreUnavailable}` wire
vocabulary, including decoded-outcome validation against the submitted operation count. `Terminal`
is reserved for real LHM admission pressure; the unavailable enum is an out-of-band lifecycle error,
never a terminal item outcome. Graph MemoryId 46 stores a tagged `Pending | Quarantined` wrapper,
prevalidates the complete pending prefix and failed row identity, then removes the acknowledged
prefix and quarantines exactly the failed row with the matching fixed pressure reason. Pending scans
and scheduling skip quarantined rows, while raw durable length and emptiness still expose them.

The additive `vector_sync_batch_outcome` endpoint maps only the real owner LHM `TablePressure`
result to the corresponding terminal variant. Nonterminal failures after a committed prefix trap so
the IC update rolls the prefix back instead of returning an ambiguous outer error. The legacy
`vector_sync_batch` endpoint remains unchanged. The live Graph IC client calls only the additive
endpoint for derived-index outbox delivery, validates each typed outcome against the submitted
count, and does not fall back after unavailable, malformed, transport, or reject failures. The
definition-pressure terminal mapping and Graph quarantine transition are proven at their owning
unit layers. Real subject pressure additionally has a live PocketIC terminal/quarantine proof. No
definition-pressure PocketIC proof is claimed because bounded sequential definition keys cannot
deterministically force a specific collision after one-hop relocation. The
fixture-only coordinated reset, compiled under `cfg(test)` / `cfg(feature = "canbench")`, preflights
both LHM owners and clears definitions, subject/deletion state, and the coupled definition-derived
regions while preserving router authority, graph ownership, shard attachments, watermarks, and the
GC cursor. A reviewed production reset operation and its rollback proof remain pending; no public
administrative endpoint exists.

## Consequences

This preserves one source of truth: the ADR fixes exact bytes, while the map derives every address
and geometry value from it. It gives ordinary reopen fixed work, restricts destructive operations to
an explicit owner path, and adds bounded resumable enumeration without an ordinary iterator or
external key catalog. It avoids an unnecessary journal and alternate-format subsystem. The accepted
cost is a deliberate pre-release stable-state wipe/reinstall before first deployment.

## Implementation and validation requirements

Implementation must prove exact header/control bytes and reserved-zero rejection; same-width schema
mismatch rejection; strict-create nonempty no-write behavior; one-grow zero-fill creation; no bucket
read in normal open; reset expected-incarnation/exhaustion/prewrite behavior; odd-epoch fail-closed
behavior; safe reuse of allocated split pages; stale/replayable bounded scrub behavior; and real
Vector MemoryId 4 and 7 owner plus IC/PocketIC rollback behavior. Physical scan must prove empty and sparse
pages, exact bucket boundaries, explicit EOF independent of short results, unchanged-map
exactly-once/replay behavior, mid-step mutation output discard, split/reset restart, serialized
cursor reopen/upgrade continuation, backward-relocation restart followed by complete fresh scan,
forward/direct/overwrite/remove non-invalidation, exact legacy-v1 restart, generation persistence
and reset behavior, overflow prewrite atomicity, malformed/zero-budget rejection, and exact
slot/read bounds.

## Design documentation impact

design/storage/linear-hash-map.md summarizes this contract, implementation status, and roadmap
without duplicating the byte-layout table. Implementation affecting Vector ownership must also
synchronize the stable-memory inventory and applicable Vector design records.

## Required axes impact

- **Encapsulation:** the map owns bytes; Vector owns its MemoryId 4 and 7 bindings and wire outcomes.
- **Separation of concerns:** Router assigns identifiers, Graph owns its outbox, and neither owns map layout.
- **Invariants:** one immutable header, one mutable control, checked derivation, epoch/incarnation
  fencing, and LHM-owned backward-relocation generation fencing.
- **Consistency:** returned errors are prewrite; post-write IC failure traps; terminal outcomes acknowledge one exact prefix.
- **Fitness for purpose:** fixed-width trusted-key point operations gain bounded reopen, reset,
  physical scan, and scrub without speculative recovery state or a duplicate key catalog.
