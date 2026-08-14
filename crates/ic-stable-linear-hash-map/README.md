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
- `get`, `get_many`, `contains_key`, `insert`, update, and `remove` inspect only those two
  candidates. `get_many` shares each small candidate page across the batch while preserving input
  order, duplicates, and misses; callers should keep batches bounded to their request budget. Live
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
  Domain-separated hash secrets are derived once from the immutable header seed and retained in
  the map handle; the read path performs no interior-mutability borrow or seed comparison. The
  immutable field is not persisted and is reconstructed from the validated header on exact open.
- For small bucket pages only, `get`, `get_many`, and `remove` allocate their first page buffer at
  its exact length and fill it through `Memory::read_unsafe`; the buffer remains operation-local
  and is reused for the second candidate with ordinary `read`. `get_many` loads each unique first
  candidate page once, then loads second candidates only for unresolved keys. The helper sets the
  vector length only after `read_unsafe` returns, with a documented proof that the allocation is
  writable, non-overlapping with stable memory, and fully initialized. The generic `Memory` default
  retains its safe zero-fill-then-read behavior, so custom implementations that do not override
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

The general canbench cases compare Linear Hash Map and `StableBTreeMap` get/insert/remove in the
same binary, using the same 4,096 successful `u64` key/value pairs, operation counts, and
`DefaultMemoryImpl` for the measured maps. This is the same cardinality used by the clustered-hash
map's general cases, so the three implementations can be compared without a fixture-size mismatch.
Probe and preflight maps use isolated `VectorMemory`, so they cannot consume the measured
stable-memory region. Setup and semantic checks remain outside the measured closures.

The persisted 4,096-entry baseline measures Linear scope instructions of 7.70M get, 24.64M insert,
and 7.30M remove (7.70M, 24.64M, and 7.30M totals). The matching `StableBTreeMap` cases measure
76.97M get, 173.82M insert, and 167.42M remove (76.97M, 173.82M, and 167.42M totals). These are
operation totals over 4,096 calls, not per-operation values; divide by 4,096 when comparing
single-operation cost. The new `get_many` diagnostics measure 9.43M instructions for all-unique
keys and 7.28M for a 64-key hot batch of 4,096 requests. The source keeps separate 48-entry phase
and 64-slot physical-scan diagnostics because those cases intentionally isolate bounded internal
work rather than general map throughput.

`canbench_results.yml` is refreshed with the unfiltered `canbench --persist` run and contains 22
entries, including the six 4,096-entry Linear/BTree comparison cases and the bounded diagnostics.

Four public-insert split fixtures use frozen literal keys and keep setup, geometry/value checks, and
reopen checks outside timing. Their current scope and total instruction values are recorded in
`canbench_results.yml`; they are intentionally separate from the 4,096-entry throughput cases.

Large-value SoA diagnostics use `u64` keys and `[u8; 2048]` values with `DefaultMemoryImpl`.
Setup, semantic checks, and reopen verification remain outside timing. These cases are diagnostics
for large payload behavior and have no corresponding clustered-hash-map throughput case.

Three additional raw component probes use one aggregate `bench_scope` per phase over all 48 items.
Prepared route inputs are created outside timing; postconditions require their reconstructed route
to equal the production route for every fixture key. These probes deliberately bypass the public
mutation epoch protocol to isolate components, so they are non-additive diagnostics and do not
replace epoch-protected direct-map totals. Mutation probes use distinct `VirtualMemory` regions
over `DefaultMemoryImpl`; their translation overhead is part of every mutation phase. Current
values remain in `canbench_results.yml`.

The source also declares three focused one-hop diagnostics (movable `u64`, exhausted, and movable
large-value), two `get_many` diagnostics (all-unique and 64-key hot batches), plus one bounded
64-physical-slot scan benchmark. Setup and semantic checks stay outside measured closures. The
`get_many` cases are intentionally diagnostic rather than a replacement for the single-key
throughput case: batching helps repeated/hot keys, while an all-unique batch pays grouping and
heap costs. Persisted benchmark results are updated only after an unfiltered `canbench --persist`
run.
