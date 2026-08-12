# ic-stable-linear-hash-map

Experimental bucketized two-choice linear hash map for Internet Computer stable memory. V1 includes
one bounded, synchronous incremental split on an absent insert that would exceed 75% load.

## Implemented V1 boundary

- Fixed-size `Storable` keys and values. Keys additionally implement `StableHashKey`, which owns
  canonical routing bytes separately from persisted key bytes.
- A 64-byte immutable header owns only layout identity: element widths, `B`, and the offsets and
  strides for control, journal, and bucket pages.
- A separate 64-byte `ControlRegion` owns mutable length, linear-hashing level/split cursors,
  physical bucket count, one persisted hash seed, split/journal state, an odd/even mutation epoch,
  and the persisted `StableHashKey::HASH_ENCODING_ID`.
- One inactive journal slot sized to `8 metadata bytes + one entry payload` remains reserved for
  future recovery work. The bounded split does not use it or persist an active phase. Reopen fails
  closed if split, journal, or mutation state is non-idle.
- The table starts at linear-hashing level 3 with 8 physical buckets, `split_cursor = 0`, and
  `B = 8` slots per bucket. Each split appends exactly one bucket, advances the cursor, and promotes
  the level at round rollover. Occupancy belongs to each bucket page.
- Each key derives two candidates in the same linear bucket universe from the persisted hash seed
  with domain separation. Insert chooses the less-loaded candidate; ties choose the first.
- `get`, `contains_key`, `insert`, update, and `remove` inspect only those two candidates. Live
  operations return `Result<_, MutationError>` so an in-progress or invalidated operation fails
  closed rather than exposing a partial snapshot.
- `new_with_hash_seed` supports deterministic placement experiments. `init_with_hash_seed` uses its
  argument only for empty memory and otherwise reopens the persisted seed. `set_hash_seed` is
  permitted only while the map is empty.
- `StableHashKey` requires `Eq`, canonical bytes stable across upgrades, platforms, and compiler
  versions, and equal keys to produce identical bytes. `HASH_ENCODING_ID` identifies that routing
  encoding and is validated on reopen. V1 routing identity is additionally frozen by layout version
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
  Domain-separated hash secrets are cached with their seed tag and refreshed from the fresh control
  seed after guard acquisition. The cache borrow ends before stable reads, decodes, comparisons, and
  writes.
- For small bucket pages only, `get` and `remove` allocate their first page buffer at its exact
  length and fill it through `Memory::read_unsafe`; the buffer remains operation-local and is
  reused for the second candidate with ordinary `read`. The helper sets the vector length only
  after `read_unsafe` returns, with a documented proof that the allocation is writable,
  non-overlapping with stable memory, and fully initialized. The generic `Memory` default retains
  its safe zero-fill-then-read behavior, so custom implementations that do not override
  `read_unsafe` preserve the same observable result and read accounting. Large-value lookup keeps
  the occupancy-plus-key fallback.

An overwrite never splits. An absent insert below 75% load uses the existing two candidates. At or
above 75% load it plans exactly the next linear split, redistributes at most the source bucket's
eight entries under the post-split routes, and then places the requested entry. If its two
post-split candidates are still full, it returns `MutationError::TablePressure` without growing,
splitting, or advancing the epoch. V1 does not iterate, recover an interrupted generic-memory write,
shrink, or provide migration compatibility. This remains an experimental storage foundation, not
yet integrated into a canister owner.

## Validation

```bash
cargo test -p ic-stable-linear-hash-map --lib
cargo clippy -p ic-stable-linear-hash-map --all-targets --all-features -- -D warnings
```

Focused canbench runs compare Linear Hash Map and `StableBTreeMap` get/insert/remove in the same
binary, using the same 48 successful `u64` key/value pairs, operation counts, and
`DefaultMemoryImpl`. Setup and semantic checks remain outside the measured closures. The current
non-persist bounded-split optimization run measured Linear scope instructions of 96.68K get, 175.26K
insert, and 89.44K remove (97.68K, 176.27K, and 90.45K totals). Against the immediately preceding
bounded-split diagnostic run, these scopes improved 3.75%, 4.25%, and 5.33% respectively. The prior
foundation run measured `StableBTreeMap`
scope instructions of 375.19K get, 1.18M insert, and 1.09M remove (376.19K, 1.19M, and 1.09M
totals); B-tree was not rerun for this split slice. There is no persisted result artifact, so these
runs are diagnostic rather than an accepted baseline. Earlier Linear figures are historical only.

Four public-insert split fixtures use frozen literal keys and keep setup, geometry/value checks, and
reopen checks outside timing. The current non-persist optimization run measured scope instructions
of 3.75K for zero moves, 4.09K for four moves, 3.75K for eight moves, and 4.05K for round rollover
(4.76K, 5.09K, 4.76K, and 5.05K totals). These are diagnostic results, not a persisted baseline.

Three additional raw component probes use one aggregate `bench_scope` per phase over all 48 items.
The current raw get probes measured 129.63K total instructions: 3.172K seed, 33.93K route, and
51.83K bucket. Insert measured 93.54K total: 55.71K control, 20.16K payload, and 13.99K metadata.
Remove measured 96.68K total: 80.50K control and 14.09K metadata. Disjoint get-route diagnostics
were key encoding 3.27K, cache borrow/seed check 2.83K, first hash 9.73K, second hash 9.65K, and
bucket mapping 2.69K instructions. Prepared route inputs are created outside timing; postconditions
require their reconstructed route to equal the production route for every fixture key. These probes
deliberately bypass the public mutation epoch protocol to isolate components, so they are
non-additive diagnostics and do not replace epoch-protected direct-map totals. Mutation probes use
distinct `VirtualMemory` regions over `DefaultMemoryImpl`; their translation overhead is part of
every mutation phase.

This initial slice does
not create a
`canbench_results.yml`; an explicitly requested unfiltered `canbench --persist` would establish the
first checked-in baseline.
