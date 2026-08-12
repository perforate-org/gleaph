# ic-stable-linear-hash-map

Experimental fixed-geometry bucketized two-choice linear hash map for Internet Computer stable
memory. Incremental linear splitting remains planned.

## Implemented V1 boundary

- Fixed-size `Storable` keys and values. Keys additionally implement `StableHashKey`, which owns
  canonical routing bytes separately from persisted key bytes.
- A 64-byte immutable header owns only layout identity: element widths, `B`, and the offsets and
  strides for control, journal, and bucket pages.
- A separate 64-byte `ControlRegion` owns mutable length, linear-hashing level/split cursors,
  physical bucket count, one persisted hash seed, split/journal state, an odd/even mutation epoch,
  and the persisted `StableHashKey::HASH_ENCODING_ID`.
- One inactive journal slot sized to `8 metadata bytes + one entry payload` is allocated for future
  recovery work. Its first eight bytes will own future journal progress; V1 has no journal protocol.
  Reopen fails closed if split, journal, or mutation state is non-idle.
- The table starts at linear-hashing level 3 with 8 physical buckets, `split_cursor = 0`, and
  `B = 8` slots per bucket. Occupancy belongs to each bucket page.
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
  constants. Changing any of those routing inputs requires an explicit rehash/layout decision.
  Stored key bytes remain owned by `Storable`; insert validates the exact persisted key and value
  widths before it acquires the mutation epoch.
- A mutator pre-encodes its key/value payloads and routing bytes, then acquires the persisted epoch
  with an even-to-odd transition before it reads live control, decodes a stored key/value, compares
  keys, or publishes a write. It publishes the next even epoch on success and ordinary errors such
  as `TablePressure`; epoch exhaustion fails before changing control. A panic leaves an odd epoch,
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

V1 does not resize, relocate, iterate, recover, or provide migration compatibility. An insert returns
`MutationError::TablePressure` only when both candidate buckets for that key are full. This is an
experimental storage foundation, not yet integrated into a canister owner.

## Validation

```bash
cargo test -p ic-stable-linear-hash-map --lib
cargo clippy -p ic-stable-linear-hash-map --all-targets --all-features -- -D warnings
```

Focused canbench runs compare Linear Hash Map and `StableBTreeMap` get/insert/remove in the same
binary, using the same 48 successful `u64` key/value pairs, operation counts, and
`DefaultMemoryImpl`. Setup and semantic checks remain outside the measured closures. The current
non-persist epoch-protected run measured Linear scope instructions of 80.62K get, 146.18K insert,
and 76.48K remove (81.63K, 147.18K, and 77.49K totals). The same run measured `StableBTreeMap`
scope instructions of 375.19K get, 1.18M insert, and 1.09M remove (376.19K, 1.19M, and 1.09M
totals). There is no persisted result artifact, so this run is diagnostic rather than an accepted
baseline. The prior epoch-free Linear figures of 78.89K get, 139.60K insert, and 72.25K remove are
retained only as historical comparisons.

Three additional raw component probes use one aggregate `bench_scope` per phase over all 48 items.
The current get phase measured 122.44K total instructions: 3.172K seed, 26.73K route, and 51.83K
bucket. The insert phase measured 83.99K total: 46.16K control, 20.16K payload, and 13.99K
metadata. The remove phase measured 84.30K total: 68.11K control and 14.09K metadata. Disjoint
get-route diagnostics were key encoding 3.27K, cache borrow/seed check 2.83K, first hash 9.73K,
second hash 9.65K, and bucket mapping 2.69K instructions. Prepared route inputs are created outside
timing; postconditions require their reconstructed route to equal the production route for every
fixture key. These probes deliberately bypass the public mutation epoch protocol to isolate
components, so they are non-additive diagnostics and do not replace epoch-protected direct-map
totals. Mutation probes use distinct `VirtualMemory` regions over `DefaultMemoryImpl`; their
translation overhead is part of every mutation phase.

This initial slice does
not create a
`canbench_results.yml`; an explicitly requested unfiltered `canbench --persist` would establish the
first checked-in baseline.
