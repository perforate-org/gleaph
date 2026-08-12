# Stable Linear Hash Map

Status: **Partially Implemented (experimental foundation)**
Last updated: 2026-08-12
Anchor timestamp: 2026-08-12 10:20:16 UTC +0000

## Purpose and ownership

`ic-stable-linear-hash-map` owns a stable-memory point map experiment for fixed-width keys and
values. The crate owns its persisted bytes, hashing policy, mutation invariants, and reopen
validation. It has no Router, Graph, index, or canister owner yet.

The full target is bucketized two-choice linear hashing with incremental splitting and
crash-recoverable mutations. Only the fixed-geometry foundation described below is implemented;
incremental linear split transitions remain planned.

## Persisted layout

All integers are little-endian.

1. An immutable 64-byte `Header` stores `LHM` magic/version, key and value widths, `B`, and the
   offsets or strides for the control region, journal slot, bucket pages, entries, and bucket pages.
2. A separate 64-byte `ControlRegion` is the single mutable metadata owner. Its byte offsets are:
   `0..8` `len`; `8` `level`; `9` split state; `10` journal state; `11..16` reserved zero;
   `16..24` `split_cursor`; `24..32` physical bucket count; `32..40` hash seed; `40..48`
   split work cursor; `48..56` mutation epoch; and `56..64` stable-hash encoding ID.
3. One inactive `JournalSlot` reserves 8 metadata bytes plus one key/value entry payload. Its first
   eight bytes, not `ControlRegion`, will own future journal progress. `ControlRegion.journal_state`
   remains the authoritative journal-state indicator.
4. Bucket pages own occupancy. Each page begins with an 8-byte header whose low 8 occupancy bits
   correspond to `B = 8` fixed-width entries; remaining header bits/bytes must be zero in V1.

`init` rejects mismatched layout identity, element widths, hash-encoding identity, impossible
geometry, occupancy/length disagreement, nonzero reserved bucket-header bytes, and non-idle split,
journal, or mutation state. Recovery is planned, so V1 fails closed instead of guessing how to
resume.

## Stable key-routing contract

`StableHashKey` extends `Storable + Eq`. `Storable` owns persisted key payload bytes; the associated
`HashBytes` representation owns canonical bytes used only for routing. The contract requires equal
keys to return identical hash bytes, and requires those bytes to be stable across upgrades,
platforms, and compiler versions. `HASH_ENCODING_ID` identifies that byte encoding and is persisted
at control bytes `56..64`; reopen rejects a different ID even if key widths are equal.

The unsigned primitive implementations return fixed-width big-endian arrays and use distinct frozen
IDs per type. Big-endian `u64` is deliberately retained to preserve V1's literal routing vectors and
stored-route expectations. V1 routing algorithm identity is frozen by layout version 1, RapidHash V3
exact mode, the two domain constants, and the literal route/reopen-byte vectors in the test suite.
Changing a key encoding, ID, hash algorithm/version, domain constant, or route rule requires an
explicit rehash/layout decision before existing memory can be reopened. Hash collisions remain valid:
the map decodes stored `Storable` bytes and uses `Eq` for key identity.

## Implemented fixed-geometry slice

- `level = 3`, `split_cursor = 0`, and 8 physical buckets at creation.
- One persisted `hash_seed`; two candidate hashes use domain separation and the same linear bucket
  universe.
- `get`, `contains_key`, insert/overwrite, remove, and remove/reinsert inspect only the two candidate
  buckets. Live query/control APIs return `Result<_, MutationError>`.
- A new entry chooses the less-loaded candidate; ties choose the first. `TablePressure` is returned
  before writes when both candidate buckets are full, even if unrelated buckets remain free.
- `new_with_hash_seed` sets a seed for fresh memory. `init_with_hash_seed` uses its argument only
  when memory is empty; existing memory always reopens the persisted seed. `set_hash_seed` is
  allowed only while the map is empty.
- After reopen validates the control region, persisted control bytes remain the canonical source for
  live length and hash seed. A mutator first serializes its key storage bytes, canonical routing
  bytes, and value storage bytes, and validates fixed key/value widths. Only then does it read the
  current even epoch, write its odd in-progress epoch, and take a fresh length-plus-seed snapshot.
  Stored decoding and `Eq` happen only while that guard is held; payload writes use the prepared
  bytes and cannot invoke user serialization. A successful mutation and every ordinary recoverable
  error, including `TablePressure` and `HashSeedNonEmpty`, publishes the next even epoch. Exhaustion
  returns `EpochExhausted` before control changes. A panic intentionally leaves the epoch odd, so
  V1 reopen returns `RecoveryRequired` until journal recovery is implemented.
- Reads first reject an odd epoch and validate that the same even epoch remains after their lookup.
  Thus a completed nested mutation during `stable_hash_bytes`, decoding, or equality comparison
  returns `MutationError::InProgress` instead of a stale result. The persisted protocol covers
  separately opened map handles backed by aliased `Memory`; it does not assert atomic behavior that
  the `Memory` implementation itself does not provide for physical concurrent writers. Derived
  domain-separated hash secrets are cached with their seed tag in a `RefCell` and refreshed from the
  fresh guarded seed. The cache borrow covers only candidate hashing and ends before stable reads,
  decodes, equality comparison, or writes.
- `get` and `remove` share a bounded value lookup. `get` reads the persisted seed and `remove`
  reads one length-plus-seed control snapshot. For buckets whose full fixed-entry payload is at
  most 1024 bytes, the lookup reads one page containing that bucket's header and entries per
  candidate. It returns the matched value and, for `remove`, its bucket, slot, and occupancy so
  remove publishes only occupancy and length after lookup. Larger bucket payloads retain the
  occupancy-plus-key scan and read only the matched value; this avoids bulk-reading large values.
  For the first small-candidate page only, the operation allocates its exact
  buffer length and calls `Memory::read_unsafe`, then sets the vector length only after that read
  initializes every byte. The helper's safety proof establishes writable allocation capacity and
  non-overlap with the stable-memory source; its result stays operation-local. The second candidate
  reuses that initialized buffer with ordinary `read`. A generic `Memory` that retains the trait
  default for `read_unsafe` still zero-fills and delegates to `read`, preserving behavior and exact
  read accounting; the large-value fallback is unchanged.
- Unit tests cover exact layout, frozen literal routes/reopen bytes, the routing-versus-storage key
  distinction, primitive big-endian encodings and distinct IDs, reopen encoding rejection,
  odd-epoch fail-closed behavior, completed nested-read invalidation, reentrant mutation rejection,
  epoch exhaustion, malformed fixed-width serialization with unchanged bytes/epoch, CRUD, overwrite,
  pressure atomicity, remove/reinsert, invalid occupancy/length, incompatible types, magic/version
  rejection, and exact bounded read/write-call accounting for first-/second-candidate and miss small
  gets, first-/second-candidate removes, and large-value lookup/remove fallback.
- Focused canbench source/config compares Linear Hash Map and `StableBTreeMap` get/insert/remove in
  the same binary, using the same 48 successful `u64` key/value pairs, operation counts, and
  `DefaultMemoryImpl`. Fixture setup and semantic pre/post checks are outside measured closures. The
  current non-persist epoch-protected run measured Linear scope instructions of 80.62K get, 146.18K
  insert, and 76.48K remove (81.63K, 147.18K, and 77.49K totals). The same run measured
  `StableBTreeMap` scope instructions of 375.19K get, 1.18M insert, and 1.09M remove (376.19K,
  1.19M, and 1.09M totals). There is no persisted result artifact, so this run is diagnostic rather
  than an accepted baseline. The prior epoch-free Linear figures of 78.89K get, 139.60K insert, and
  72.25K remove are retained only as historical comparisons.
- Canbench-only raw component probes put one aggregate scope around each phase's 48 items. The
  current get phase measured 122.44K total instructions: 3.172K seed, 26.73K route, and 51.83K
  bucket. The insert phase measured 83.99K total: 46.16K control, 20.16K payload, and 13.99K
  metadata. The remove phase measured 84.30K total: 68.11K control and 14.09K metadata. Disjoint
  get-route diagnostics were key encoding 3.27K, cache borrow/seed check 2.83K, first hash 9.73K,
  second hash 9.65K, and bucket mapping 2.69K instructions. Prepared route inputs are created
  outside timing; postconditions reconstruct every prepared route and require it to equal the
  production route for all 48 keys. Raw probes deliberately bypass the public epoch guard to isolate
  components, so they are non-additive diagnostics and do not decompose the epoch-protected product
  operations. Mutation probes use distinct `VirtualMemory` regions over `DefaultMemoryImpl`;
  memory-translation overhead is part of every mutation phase. Probe APIs are crate-private and
  compiled only for wasm canbench; the product API and persisted format are unchanged.

## Planned full design

The following remains planned and must not be inferred from the current API:

1. Incremental linear split transitions that advance `split_cursor`, allocate exactly one new
   bucket, and promote `level` after completing a round.
2. Split admission and bounded work that prevent a public mutation from scanning or moving an
   unbounded number of entries.
3. A journal protocol with explicit prepare/apply/commit/recover states, exact ownership of the
   staged payload, operation-atomic error behavior, and reopen recovery tests at every persisted
   boundary.
4. Capacity growth, checked byte arithmetic, OOM atomicity, and stable-memory page accounting.
5. Iteration/resume semantics while splits are active.
6. Format migration and compatibility policy. The experimental V1 makes no compatibility promise.
7. Matched benchmarks against clustered hashing, acceptance thresholds, persisted canbench
   artifacts, and an owning production workload. The implemented B-tree comparison is only the
   initial same-input point-operation reference.

Production integration requires a separate reviewed plan or ADR covering these boundaries. Until
then, this crate remains an isolated experiment.
