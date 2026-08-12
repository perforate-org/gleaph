# Stable Linear Hash Map

Status: **Partially Implemented (experimental bounded-split foundation)**
Last updated: 2026-08-12
Anchor timestamp: 2026-08-12 14:39:52 UTC +0000

## Purpose and ownership

`ic-stable-linear-hash-map` owns a stable-memory point map experiment for fixed-width keys and
values. The crate owns its persisted bytes, hashing policy, mutation invariants, and reopen
validation. It has no Router, Graph, index, or canister owner yet.

The implemented target is bucketized two-choice linear hashing with one synchronous bounded split
per absent insert. Durable journal recovery, iteration, migration, and production integration remain
planned.

## Persisted layout

All integers are little-endian.

1. An immutable 64-byte `Header` stores `LHM` magic/version, key and value widths, `B`, and the
   offsets or strides for the control region, journal slot, and bucket pages. Header bytes `48..56`
   store `value_slab_offset = 8 + B * key_size`; bytes `56..64` store
   `bucket_page_stride = value_slab_offset + B * value_size`.
2. A separate 64-byte `ControlRegion` is the single mutable metadata owner. Its byte offsets are:
   `0..8` `len`; `8` `level`; `9` split state; `10` journal state; `11..16` reserved zero;
   `16..24` `split_cursor`; `24..32` physical bucket count; `32..40` hash seed; `40..48`
   split work cursor; `48..56` mutation epoch; and `56..64` stable-hash encoding ID.
3. One inactive `JournalSlot` reserves 8 metadata bytes plus one key/value entry payload. Its first
   eight bytes, not `ControlRegion`, will own future journal progress. `ControlRegion.journal_state`
   remains the authoritative journal-state indicator.
4. Bucket pages own occupancy and use a structure-of-arrays layout. Each page begins with an 8-byte
   header whose low 8 occupancy bits correspond to `B = 8` slots; remaining header bits/bytes must
   be zero. The key slab contains `B` fixed-width keys at `8 + slot * key_size`. The value slab then
   contains `B` fixed-width values at `value_slab_offset + slot * value_size`. Keys and values for a
   slot are paired by the shared occupancy bit and slot index, not adjacency.

`init` rejects mismatched layout identity, element widths, hash-encoding identity, impossible
geometry, occupancy/length disagreement, nonzero reserved bucket-header bytes, and non-idle split,
journal, or mutation state. Recovery is planned, so V1 fails closed instead of guessing how to
resume.

The SoA format is fresh-memory-only. This experimental V1 provides no compatibility reader or
migration for earlier AoS bytes; opening bytes whose header does not exactly match the SoA-derived
offsets and strides fails with `InvalidLayout`.

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
Dynamic power-of-two bucket reduction uses equivalent bit masks, checked against modulo at geometry
boundaries plus deterministic samples. Changing a key encoding, ID, hash algorithm/version, domain
constant, or route rule requires an
explicit rehash/layout decision before existing memory can be reopened. Hash collisions remain valid:
the map decodes stored `Storable` bytes and uses `Eq` for key identity.

## Implemented bounded-split slice

- `level = 3`, `split_cursor = 0`, and 8 physical buckets at creation. Settled geometry is
  `physical_buckets = 2^level + split_cursor`; reopen accepts valid mid-round and rollover states.
- One persisted `hash_seed`; two candidate hashes use domain separation and the same linear bucket
  universe.
- `get`, `contains_key`, insert/overwrite, remove, and remove/reinsert inspect only the two candidate
  buckets. Live query/control APIs return `Result<_, MutationError>`.
- A new entry chooses the less-loaded candidate; ties choose the first. The insert that reaches
  exactly 75% capacity stays direct. A later absent insert plans exactly one next-in-order split,
  appends one bucket, recomputes the source bucket's at most eight entries under post-split routing,
  and advances the cursor or rolls the level. Entries that remain source-addressable stay in their
  original slots; only entries that would become unreachable move to the appended bucket.
  `TablePressure` is returned before growth or writes if the requested post-split pair is still full.
- `new_with_hash_seed` sets a seed for fresh memory. `init_with_hash_seed` uses its argument only
  when memory is empty; existing memory always reopens the persisted seed. `set_hash_seed` is
  allowed only while the map is empty.
- After reopen validates the control region, persisted control bytes remain the canonical source for
  live length, geometry, hash seed, and mutation epoch. Insert planning reads one full authoritative
  control snapshot, rejects its odd epoch, serializes requested bytes, decodes stored keys/values,
  runs equality and stable-hash callbacks, checks arithmetic, and constructs complete final source/new
  pages. A successful apply revalidates that observed even epoch exactly once immediately before any
  logical write. A planning error also revalidates it before return, so an alias mutation supersedes
  a stale `TablePressure` or capacity error. A split grows one appended bucket before acquiring the
  exact-epoch guard. `TablePressure`, `OutOfMemory`, `CapacityOverflow`, encoding errors, and epoch
  mismatch therefore leave logical bytes and the mutation epoch unchanged; successful growth followed
  by alias invalidation may leave only unreachable zero capacity. The guarded apply phase performs
  only prepared writes: final source page, final new page, any target in an unaffected candidate,
  settled geometry/length, then the next even epoch. A panic intentionally leaves the epoch odd, so
  reopen returns `RecoveryRequired`.
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
- Remove planning reads occupancy and only the key slab while searching. It reads/decodes a value
  only for the exact matched slot, before acquiring the mutation guard. The apply phase therefore
  writes occupancy and length only; it performs no user-controlled decode after the epoch is odd.
- Unit tests cover exact layout, frozen literal routes/reopen bytes, mask-versus-modulo routing at
  every valid level's geometry boundaries plus deterministic samples, the routing-versus-storage key
  distinction, primitive big-endian encodings and distinct IDs, reopen encoding rejection,
  odd-epoch fail-closed behavior, completed nested-read invalidation, reentrant mutation rejection,
  epoch exhaustion, malformed fixed-width serialization with unchanged bytes/epoch, CRUD, overwrite,
  pressure atomicity, remove/reinsert, invalid occupancy/length, incompatible types, magic/version
  rejection, and exact bounded read/write-call accounting for direct and overwrite insert planning,
  first-/second-candidate and miss small gets, first-/second-candidate removes, and large-value
  lookup/remove fallback.
- Focused canbench source/config compares Linear Hash Map and `StableBTreeMap` get/insert/remove in
  the same binary, using the same 48 successful `u64` key/value pairs, operation counts, and
  `DefaultMemoryImpl`. Fixture setup and semantic pre/post checks are outside measured closures. The
  first persisted SoA baseline measures Linear scope instructions of 96.07K get, 163.35K insert, and
  80.33K remove (97.07K, 164.36K, and 81.34K totals). The prior AoS run of these same named benches
  measured 96.68K, 175.26K, and 89.44K scopes (97.68K, 176.27K, and 90.45K totals). The persisted
  artifact measures `StableBTreeMap` scopes of 374.98K get, 1.186M insert, and 1.086M remove
  (375.99K, 1.187M, and 1.087M totals). The artifact contains all 16 benchmark keys currently
  declared in `src/bench.rs`; the prior AoS Linear values remain explicitly historical.
- Four public-insert split fixtures freeze literal key sets for zero, four, and eight moved source
  entries and for level rollover. Setup plus exact geometry, length, all-resident values, requested
  value, and reopen checks remain outside timing. The persisted SoA baseline measures scope/total
  instructions of 3.60K/4.60K for zero moves, 3.89K/4.90K for four moves, 3.60K/4.60K for eight
  moves, and 3.86K/4.87K for round rollover.
- New large-value SoA diagnostics use `u64` / `[u8; 2048]` with `DefaultMemoryImpl`. Sixteen
  contains misses measured 33.64K scope / 35.12K total instructions, sixteen get hits measured
  239.30K / 242.31K, and a public split moving four large values measured 117.00K / 118.00K. Setup,
  semantic checks, and reopen verification remain outside timing. These new named benches have no
  comparable AoS baseline. These results are included in the persisted artifact.
- Canbench-only raw component probes put one aggregate scope around each phase's 48 items. The
  persisted get phase measures 128.86K total instructions: 3.172K seed, 33.93K route, and 51.06K
  bucket. The insert phase measures 99.35K total: 61.57K control, 20.12K payload, and 13.99K
  metadata. The remove phase measures 102.97K total: 86.78K control and 14.09K metadata. Disjoint
  get-route diagnostics were key encoding 3.27K, cache borrow/seed check 2.83K, first hash 9.73K,
  second hash 9.65K, and bucket mapping 2.69K instructions. Prepared route inputs are created
  outside timing; postconditions reconstruct every prepared route and require it to equal the
  production route for all 48 keys. Raw probes deliberately bypass the public epoch guard to isolate
  components, so they are non-additive diagnostics and do not decompose the epoch-protected product
  operations. Mutation probes use distinct `VirtualMemory` regions over `DefaultMemoryImpl`;
  memory-translation overhead is part of every mutation phase. Probe APIs are crate-private and
  compiled only for wasm canbench; the product API and persisted format are unchanged.

## Planned follow-up design

The following remains planned and must not be inferred from the current API:

1. A journal protocol with explicit prepare/apply/commit/recover states, exact ownership of the
   staged payload, operation-atomic error behavior, and reopen recovery tests at every persisted
   boundary, only if a later mutation can no longer complete in one bounded update call.
2. Iteration/resume semantics across settled linear geometry.
3. Format migration and compatibility policy. The experimental V1 makes no compatibility promise.
4. Matched benchmarks against clustered hashing, acceptance thresholds, and an owning production
   workload. The implemented B-tree comparison and persisted artifact are only the initial
   same-input point-operation reference.

Production integration requires a separate reviewed plan or ADR covering these boundaries. Until
then, this crate remains an isolated experiment.
