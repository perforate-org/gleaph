# Stable Linear Hash Map

Status: **pre-release V1 implementation**
Contract anchor: 2026-08-14 21:52:20 UTC +0000

This crate is the final V1 implementation for fixed-width stable-memory maps used by Gleaph.
V1 is a destructive replacement: a nonempty region must contain this exact format or `open` fails.
There is no migration reader, compatibility format, fallback create, or V2 branch. Deployment
before first production use may wipe and recreate the owned region.

## Layout

- Header: 128 bytes, including the literal `GLEAPH-LHM-V1` format fingerprint.
- Control: 64 bytes at byte 128. It stores length, physical bucket count, even/odd mutation epoch,
  incarnation, split debt, and the count of entries in inline overflow pages.
- Each logical bucket is one fixed block: one primary page plus two inline overflow pages. Every
  page has an occupancy word followed by fixed-width key and value slabs.
- Stable memory is one map-owned region. Overflow pages are part of the bucket block; there is no
  second arena, free-list, compatibility layout, or external key catalog.

The immutable header owns key/value widths, type-owned schema identities, and the hash seed.
Level and split cursor are derived from the persisted physical bucket count. All offsets and
extents use checked arithmetic.

## Operations

`get`, `get_many`, `contains_key`, `insert`, and `remove` use two hash candidates. Point operations
read only the candidate bucket blocks and fence the result with the persisted mutation epoch.
`get_many` is a bounded convenience for repeated lookups; it does not change the single-key
contract.

An absent insert is admitted in this order:

1. Use a free slot in either candidate block, including its inline overflow pages.
2. If both blocks are full, plan the next standard linear-hash split and place the key in the
   post-split images.
3. If the split cannot admit the key, return `TablePressure` without changing logical bytes.

An overflow insertion records split debt. `maintenance_step(entry_budget, byte_budget)` services
multiple debt items in split-pointer order until either debt is cleared or the work budget is
consumed. A `Pending` result means the next complete split needs more budget; it is not a table
failure. The split planner moves a bounded source block, reserves the required stable extent before
the first logical write, and publishes the new geometry only after all planned block images exist.
`TablePressure` is reserved for a genuine bounded-admission failure, capacity overflow, or a stable
memory limit; ordinary bucket fullness is absorbed by overflow or maintenance.

`scan_start` and `scan_step` expose bounded physical enumeration without an ordinary iterator or a
second catalog. The cursor is exactly 88 bytes and contains schema identities, seed, incarnation,
physical bucket count, and the next physical slot. Each call budgets physical slots, reports the
exact examined count, and has an explicit `exhausted` flag. Split or reset invalidates a cursor;
ordinary same-geometry mutations do not claim a multi-call snapshot. `scrub_snapshot` and
`scrub_step` are separate bounded integrity checks.

Strict `create` accepts only zero-sized memory. `init_with_hash_seed` creates only at size zero and
otherwise exact-opens the persisted seed. `reset(expected_incarnation)` is an explicit owner
operation that preflights the incarnation, epoch, and initial extent before clearing the initial
bucket blocks. It never repairs an incompatible region.

## Validation and comparison benchmark

```text
cargo test -p ic-stable-linear-hash-map --lib
cargo clippy -p ic-stable-linear-hash-map --all-targets --all-features -- -D warnings
```

The canbench fixture uses the same 4,096 successful `u64 -> u64` entries for Linear Hash Map and
`StableBTreeMap`. It compares get, insert, and remove totals with setup outside the measured
closure, and includes one bounded split-debt maintenance case. No probe map shares the measured
stable-memory region, so the fixture cannot trap with `NonEmptyMemory` during initialization.

The selected two-overflow-page V1 layout is persisted in the benchmark artifact. Against the
previous four-overflow-page result, the 4,096-entry run measured Linear get 17.82M instructions
(-33.78%), insert 61.96M (-40.61%), remove 20.77M (-33.08%), and maintenance 11.25M (+7.03%).
The BTree comparison was unchanged; Linear insert and maintenance used one and two additional
stable-memory pages respectively, while get and remove were unchanged.
