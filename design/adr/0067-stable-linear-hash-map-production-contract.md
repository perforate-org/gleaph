# 0067. Stable linear hash map production contract

Date: 2026-08-12
Status: proposed
Last revised: 2026-08-13
Anchor timestamp: 2026-08-13 12:28:04 UTC +0000

> **Proposed contract.** This ADR is the sole exact V1 persisted-format contract for
> ic-stable-linear-hash-map. It is deliberately ahead of the current implementation and does not
> declare the crate or a consumer production-ready.

## Context

ic-stable-linear-hash-map owns one stable-memory region for fixed-width point-map operations. The
first intended consumer is Vector VECTOR_INDEX_DEFS, keyed by Router-assigned u32 and owned by the
Vector canister at MemoryId 4. Router assigns identifiers and authorizes work; Vector owns its one
map binding and domain outcomes; the generic map owns persisted bytes and their invariants.

ADR 0007 remains authoritative for the stable-region inventory. ADR 0064 permits a destructive
pre-release Vector reinstall. This ADR permits neither an alternate reader nor conversion of a
nonempty region.

## Problem

The map needs one inspectable byte contract, fixed-cost normal reopening, an explicit destructive
reset, and a bounded full-integrity operation. Width-only checks are unsafe because equal widths do
not establish compatible key storage, key routing, or value storage. Scanning every allocated bucket
on open makes upgrade cost grow with table history.

## Existing architecture assessment

The existing MemoryManager/VirtualMemory boundary is sufficient. A new registry, manager, or
Router-owned storage facade would duplicate state ownership. The map crate alone owns layout,
schema validation, address arithmetic, routing, split geometry, mutation fencing, reset, and scrub.
The Vector facade alone owns MemoryId 4, staged availability, and map-error translation. Graph and
Router never read or write raw map bytes.

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

## Decision

### 1. One destructive pre-release V1 format

The only supported layout has magic LHM and version exactly 1. V1 is destructive and
fresh-memory-only: strict create accepts only memory.size() == 0; any nonempty region must pass
exact V1 open validation or is rejected without writes. There is no alternate reader, conversion,
fallback create, or reset-on-open-error behavior.

All integers are unsigned little-endian. Every reserved byte is written and validated as zero.
Giving a reserved byte meaning requires a future explicit format decision, not a reinterpretation
under V1.

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
| 160..192 | [u8; 32] | reserved | all zero |

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
final publication. Initial control is len = 0, physical_buckets = 8, mutation_epoch = 0, and
incarnation = 1; the header receives the trusted creation seed.

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
pages, and publishes the final even control snapshot last.

This map-local reset operation is implemented in the V1 collection crate. Vector integration is
not: the Vector owner must coordinate this operation with every other Vector-owned stable region in
one update before exposing a domain reset API.

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

An absent insert may perform at most one next-in-order bounded split. TablePressure means both
candidate buckets remain full after that split. It is a typed terminal admission outcome, not
allocation failure or a generic retry.

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

### 7. Vector and Graph boundary

Only the Vector facade binds VECTOR_INDEX_DEFS to MemoryId 4. Its lifecycle is Uninitialized to
Ready or Uninitialized to Unavailable(reason); static construction does not open memory. Fresh
install strict-creates with the trusted seed. Post-upgrade exact-opens a nonempty region without a
seed argument. An authorized reset is a separate operation. Unavailable and uninitialized
reads/writes return a typed unavailable result and never infer empty state.

Vector maps terminal pressure to IndexDefinitionTablePressure and map availability failure to
IndexDefinitionStoreUnavailable. It preflights each definition before coupled writes. Its batch
outcome distinguishes a committed prefix from one terminal item:

    VectorSyncBatchOutcome =
      Progress { applied: u32 }
      | Terminal {
          applied: u32,
          failed_index: u32,
          error: IndexDefinitionTablePressure,
        }

Terminal requires failed_index == applied < operations.len(). Graph removes exactly the
acknowledged prefix and quarantines exactly the failed item in one update, or changes neither.
Transport, decode, malformed reply, and unavailable outcomes keep the submitted outbox slice
unchanged.

**Implementation status (2026-08-13):** `gleaph-graph-kernel` now defines the additive shared
`VectorSyncBatchOutcome`, `IndexDefinitionTablePressure`, and
`IndexDefinitionStoreUnavailable` wire vocabulary, including decoded-outcome validation against the
submitted operation count. `Terminal` exclusively carries `IndexDefinitionTablePressure`;
`IndexDefinitionStoreUnavailable` is an out-of-band lifecycle error, never a terminal item outcome.
This does not change the live `vector_sync_batch` endpoint, which still returns
`VectorSyncBatchProgress`, or Graph's legacy outbox behavior; it does not implement outbox removal,
quarantine, definition-store lifecycle, linear-hash-map integration, or migration.

## Consequences

This preserves one source of truth: the ADR fixes exact bytes, while the map derives every address
and geometry value from it. It gives ordinary reopen fixed work, restricts destructive operations to
an explicit owner path, and avoids an unnecessary journal, alternate-format subsystem, or iterator.
The accepted cost is a deliberate pre-release stable-state wipe/reinstall before first deployment.

## Implementation and validation requirements

Implementation must prove exact header/control bytes and reserved-zero rejection; same-width schema
mismatch rejection; strict-create nonempty no-write behavior; one-grow zero-fill creation; no bucket
read in normal open; reset expected-incarnation/exhaustion/prewrite behavior; odd-epoch fail-closed
behavior; safe reuse of allocated split pages; stale/replayable bounded scrub behavior; and real
Vector MemoryId 4 plus IC/PocketIC rollback behavior.

## Design documentation impact

design/storage/linear-hash-map.md summarizes this contract, implementation status, and roadmap
without duplicating the byte-layout table. Implementation affecting Vector ownership must also
synchronize the stable-memory inventory and applicable Vector design records.

## Required axes impact

- **Encapsulation:** the map owns bytes; Vector owns its one binding and wire outcomes.
- **Separation of concerns:** Router assigns identifiers, Graph owns its outbox, and neither owns map layout.
- **Invariants:** one immutable header, one mutable control, checked derivation, epoch fencing, and incarnation fencing.
- **Consistency:** returned errors are prewrite; post-write IC failure traps; terminal outcomes acknowledge one exact prefix.
- **Fitness for purpose:** fixed-width trusted-key point operations gain bounded reopen, reset, and scrub without speculative recovery state.
