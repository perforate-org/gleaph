# ic-stable-linear-hash-map

Experimental bucketized two-choice linear hash map for Internet Computer stable memory. V1 includes
one bounded, synchronous incremental split on an absent insert that would exceed 75% load.

## Implemented V1 boundary

- Fixed-size `Storable` keys and values. Keys additionally implement `StableHashKey`, which owns
  canonical routing bytes separately from persisted key bytes.
- The exact 128-byte immutable header owns key/value widths, type-owned key storage/routing and value
  storage identities, and the immutable hash seed. Layout offsets and strides are derived.
- The exact 64-byte `ControlRegion` owns mutable length, physical bucket count, odd/even mutation
  epoch, incarnation, and the backward-relocation generation used by physical scans. Level and
  split cursor are derived from the physical bucket count.
- The table starts at linear-hashing level 3 with 8 physical buckets, `split_cursor = 0`, and
  `B = 8` slots per bucket. Each split appends exactly one bucket, advances the cursor, and promotes
  the level at round rollover. Each page stores its 8-byte occupancy header, all eight fixed-width
  keys contiguously, then all eight fixed-width values contiguously. Slot `i` is addressed at
  `page + 8 + i * key_size` and `page + value_slab_offset + i * value_size`.
- Each key derives two candidates in the same linear bucket universe from the persisted hash seed
  with domain separation. Insert chooses the less-loaded candidate; ties choose the first.
- `get`, `contains_key`, `insert`, update, and `remove` inspect only those two candidates. Live
  operations return `Result<_, MutationError>` so an in-progress or invalidated operation fails
  closed rather than exposing a partial snapshot.
- `new_with_hash_seed` is strict create and rejects every nonempty memory without writes.
  `init_with_hash_seed` is canonical open-or-create: it creates only at size zero and otherwise
  exact-opens the persisted seed. `open` is exact-open-only and rejects empty memory. Normal open
  reads only the header and control plus memory extent.
- `StableHashKey` requires `Eq`, canonical bytes stable across upgrades, platforms, and compiler
  versions, and equal keys to produce identical bytes. `KEY_STORAGE_ID`, `KEY_ROUTING_ID`, and
  `StableMapValue::VALUE_STORAGE_ID` identify persisted encodings and are validated on reopen. V1
  routing identity is additionally frozen by layout version
  1, the literal routing vectors in the unit tests, RapidHash V3 exact mode, and the two fixed domain
  constants. Dynamic power-of-two bucket reduction uses equivalent bit masks, with modulo
  equivalence checked at geometry boundaries and deterministic samples. Changing any of those
  routing inputs requires an explicit rehash/layout decision.
  Stored key bytes remain owned by `Storable`; insert validates the exact persisted key and value
  widths before it acquires the mutation epoch.
- An insert pre-encodes its key/value payloads and routing bytes, then reads one authoritative
  persisted `ControlRegion` snapshot to plan all decoding, equality callbacks, routing, checked
  arithmetic, and final bucket images against its even epoch. A successful direct mutation
  revalidates that epoch exactly once immediately before changing it to odd; a split additionally
  grows stable memory for the appended bucket before that revalidation. A planning error still
  revalidates the observed epoch before it is returned, so an alias mutation supersedes a stale
  `TablePressure` or capacity error. Returned errors, including `TablePressure`, `OutOfMemory`, and
  `CapacityOverflow`, leave logical bytes and the epoch unchanged. The guarded apply phase performs
  writes only and publishes geometry and length before the next even epoch. A panic after the odd
  publication leaves an odd epoch,
  which makes reopen return `RecoveryRequired` until a future journal recovery protocol exists.
  Reads reject an odd epoch and compare the same even epoch before and after lookup, so a nested
  mutation that completes during `stable_hash_bytes`, decoding, or equality comparison invalidates
  the read result. This persisted protocol applies across separately opened handles over aliased
  `Memory`; it does not claim concurrent physical-memory atomicity beyond the `Memory` implementation.
  Domain-separated hash secrets are cached from the immutable header seed. The cache borrow ends
  before stable reads, decodes, comparisons, and writes.
- For small bucket pages only, `get` and `remove` allocate their first page buffer at its exact
  length and fill it through `Memory::read_unsafe`; the buffer remains operation-local and is
  reused for the second candidate with ordinary `read`. The helper sets the vector length only
  after `read_unsafe` returns, with a documented proof that the allocation is writable,
  non-overlapping with stable memory, and fully initialized. The generic `Memory` default retains
  its safe zero-fill-then-read behavior, so custom implementations that do not override
  `read_unsafe` preserve the same observable result and read accounting. Large-value lookup keeps
  the occupancy-plus-key fallback.
- `scan_start` and `scan_step` provide resumable physical enumeration without exposing an
  `Iterator` or requiring a second key catalog. The version-2 96-byte cursor encodes the three
  schema identities, immutable seed, incarnation, physical bucket bound, next physical slot, and
  backward-relocation generation; it deliberately omits epoch, length, and derived geometry. It
  therefore survives exact reopen or canister upgrade while those captured fields still match.
  Exact 88-byte version-1 cursor bytes still decode, but are deliberately stale and `scan_step`
  returns `RestartRequired`; unknown size/version combinations remain malformed. Each
  positive-budget step reads at most that many physical slots, returns entries in physical order
  with the next cursor and exact examined-slot count, and reports EOF only through `exhausted`.
  Fixed-width payloads make the slot budget an entry and encoded-output bound. The step reads an
  even mutation epoch before and after collecting its local output. A same-geometry epoch change
  returns `InProgress`; reset, split, or a backward resident relocation returns
  `RestartRequired`; neither returns mixed output. Direct insert, overwrite, remove, and forward
  relocation do not invalidate a cursor between completed steps. Exactly-once enumeration is still
  guaranteed only while the map remains unchanged across the lap.
- `scrub_snapshot` and `scrub_step` provide an explicit bounded integrity scan. The opaque,
  handle-bound cursor captures the exact schema identities, immutable seed, even mutation epoch,
  incarnation, length, and physical-bucket bound; each positive-budget step scans only its primary
  bucket range plus bounded candidate probes. It validates reserved occupancy bits, canonical
  fixed key/value encodings, route reachability, duplicate candidate placement, and final length.
  The exact fence is checked before and after every step, so an alias mutation makes the cursor
  stale and supersedes an integrity result from mixed bytes. The cursor is external to map bytes,
  replayable only on its originating open handle, and is not a wire or stable encoding. Reopen or
  upgrade starts a new scrub session at bucket zero. Fixed-width bytes that decode and re-encode
  noncanonically return typed scrub errors. A panic in user-defined `Storable` decode/encode,
  `StableHashKey` hashing, or `Eq` is recoverable only in unwind-enabled host tests; the wasm
  panic-abort build traps and the IC update boundary fails closed.
- `reset(expected_incarnation)` is a destructive owner operation, not a general `clear`. It
  preflights the ownership fence, successor incarnation, epoch pair, and initial extent before the
  first write. Success preserves the immutable header, seed, schemas, payload bytes, and trailing
  pages; it clears only the eight initial occupancy headers and publishes empty initial geometry,
  the successor incarnation, backward-relocation generation zero, and the final even epoch together
  as the last control write.

An overwrite never splits. An absent insert below 75% load uses the existing two candidates; if both
are full, it scans at most their sixteen residents in deterministic candidate/slot order and moves
only one resident to that resident's other candidate before admitting the key. At or above 75% load
it first plans exactly the next linear split, redistributes at most the source bucket's eight
entries under the post-split routes, and then places the requested entry. If that prospective target
pair is still full, it keeps the current geometry and retries current-geometry admission, including
the same one-hop relocation, before returning `MutationError::TablePressure`. The one-hop planner
prepares every resident decode, route, checked offset, source/destination page image, and epoch
fence before the odd epoch or first write. A resident move to a lower physical slot checked-advances
the persisted backward-relocation generation; `u64::MAX` rejects before any stable write. Forward
relocation leaves it unchanged. True pressure neither grows nor changes bytes, control, geometry,
or epoch. V1 exposes no ordinary iterator, recover an interrupted generic-memory write,
shrink, or migration compatibility. The bounded physical scan is not an integrity validator;
corrupted bucket pages are diagnosed through bounded scrub. The SoA layout is fresh-memory-only: V1
has no reader or migration path for earlier experimental bytes. Consumer integration remains
planned; reset is map-local only and a Vector owner must coordinate all of its stable regions in one
update before it can expose reset.

## Validation

```bash
cargo test -p ic-stable-linear-hash-map --lib
cargo clippy -p ic-stable-linear-hash-map --all-targets --all-features -- -D warnings
```

Focused canbench runs compare Linear Hash Map and `StableBTreeMap` get/insert/remove in the same
binary, using the same 48 successful `u64` key/value pairs, operation counts, and
`DefaultMemoryImpl`. Setup and semantic checks remain outside the measured closures. The first
persisted SoA baseline measures Linear scope instructions of 96.07K get, 163.35K insert, and 80.33K
remove (97.07K, 164.36K, and 81.34K totals). The prior AoS run of the same three named benches
measured 96.68K, 175.26K, and 89.44K scope instructions (97.68K, 176.27K, and 90.45K totals).
The persisted artifact measures `StableBTreeMap` scope instructions of 374.98K get, 1.186M insert,
and 1.086M remove (375.99K, 1.187M, and 1.087M totals). The persisted artifact contains the 16
baseline benchmark entries measured before the one-hop diagnostics were added; the prior AoS Linear
figures are historical only.

Four public-insert split fixtures use frozen literal keys and keep setup, geometry/value checks, and
reopen checks outside timing. The persisted SoA baseline measures scope/total instructions of
3.60K/4.60K for zero moves, 3.89K/4.90K for four moves, 3.60K/4.60K for eight moves, and
3.86K/4.87K for round rollover.

Large-value SoA diagnostics use `u64` keys and `[u8; 2048]` values with `DefaultMemoryImpl`.
Sixteen contains misses measured 33.64K scope / 35.12K total instructions; sixteen get hits measured
239.30K / 242.31K. A public split moving four large values measured 117.00K / 118.00K. Setup,
semantic checks, and reopen verification remain outside timing. These are new named benches with no
comparable AoS baseline.

Three additional raw component probes use one aggregate `bench_scope` per phase over all 48 items.
The persisted raw get probe measures 128.86K total instructions: 3.172K seed, 33.93K route, and
51.06K bucket. Insert measures 99.35K total: 61.57K control, 20.12K payload, and 13.99K metadata.
Remove measures 102.97K total: 86.78K control and 14.09K metadata. Disjoint get-route diagnostics
were key encoding 3.27K, cache borrow/seed check 2.83K, first hash 9.73K, second hash 9.65K, and
bucket mapping 2.69K instructions. Prepared route inputs are created outside timing; postconditions
require their reconstructed route to equal the production route for every fixture key. These probes
deliberately bypass the public mutation epoch protocol to isolate components, so they are
non-additive diagnostics and do not replace epoch-protected direct-map totals. Mutation probes use
distinct `VirtualMemory` regions over `DefaultMemoryImpl`; their translation overhead is part of
every mutation phase.

`canbench_results.yml` is the first unfiltered persisted baseline and contains 16 entries. The source
also declares three focused one-hop diagnostics (movable `u64`, exhausted, and movable
large-value) plus one bounded 64-physical-slot scan benchmark. Setup and semantic checks stay
outside the measured scan closure. These four benchmarks are intentionally unpersisted in this
change and are not part of that baseline.
