# ic-stable-clustered-hash-map

`StableClusteredHashMap` — a hash map in Internet Computer stable memory, using
clustered hashing (Amble & Knuth 1974, "Ordered Hash Tables") for compact storage and
fast point lookups.

The current V1 layout uses a 128-byte metadata prefix; table entries begin at byte 128.

## Operations

- `get` / `insert` / `remove` / `contains_key` in **O(1)** amortized time (by key).
- Growing when `len >= 3/4 * buckets` rehashes **incrementally**: the remap is spread
  across subsequent operations (amortized **O(1)** per op). Bucket doubling requires a settled
  remap. If relocation reaches the current logical capacity, the map first grows and clears a
  small persisted tail chunk without changing the bucket mapping or draining active remap work.
  A new-key insert that reaches the next load threshold before the remap settles continues the
  bounded remap without starting another bucket generation.
- Active remap maintenance and settled resize initialization are both bounded per mutation. A
  settled threshold insert grows and clears only a fixed prefix of the new region; the persisted
  cursor keeps old-N lookup valid until the new mapping is published.
- `insert` and `remove` return `InsertError` when their requested mutation and bounded remap
  maintenance encounter stable-memory growth or capacity failure. The whole public operation is
  failure-atomic: `OutOfMemory` and `CapacityOverflow` leave the logical map bytes, header, length,
  capacity, remap boundary, and key set unchanged and reopenable. Stable-memory pages grown before a
  later failure are physical backing and are not part of that logical rollback contract. Active
  operations use the direct path when a bounded empty suffix proves that maintenance plus the
  request cannot need growth; pending resize operations grow and clear their next prefix before
  writing, leaving an empty destination for the request. Other growth-capable operations write once
  through an undo transaction that snapshots each overwritten original logical block at most once
  and restores those blocks on a returned error. A trap relies on the Internet Computer's message
  rollback boundary; this is not a standalone write-ahead journal for process-crash recovery.
- Iteration is in unordered slot order (`iter`, and `iter_from` for resumable bounded
  scans).

## Compared to `StableBTreeMap`

`StableBTreeMap` (ic-stable-structures) is the ordered-tree baseline that motivated this
crate. Measured with canbench (`N = 4096`, per-op instructions):

| operation | clustered | btree   | clustered vs btree |
| --------- | --------- | ------- | ------------------ |
| get       | ≈ 1.4k    | ≈ 17.6k | ~12.4x faster      |
| insert    | ≈ 16.4k   | ≈ 41.7k | ~2.55x faster      |
| remove    | ≈ 3.1k    | ≈ 39.2k | ~12.6x faster      |

The threshold-trigger benchmark measures the public insert that performs the settled resize. Setup
is outside the timed closure, and each fixture stores one valid resident per old home bucket.

| old N | residents | trigger scope (instructions) | clear in timed call (`u64/u64`) |
| ----- | ---------: | ---------------------------: | -------------------------------: |
| 13    |      6,144 |                       17,574 |                       1,280 B |
| 16    |     49,152 |                       17,574 |                       1,280 B |
| 20    |    786,432 |                       17,574 |                       1,280 B |
| 23    |  6,291,456 |                       17,574 |                       1,280 B |

The measured initialization work is now a fixed 64-slot prefix per call; the values above include
the target-selected stable-memory growth and metadata overhead, not a table-sized clear. The full
N=23→N=24→N=26 resize series remains spread across later operations; these threshold fixtures do
not measure the total cost of completing that series. The Internet
Computer currently documents a 40B update-call instruction limit, a 7B per-round execution-thread
limit, and 500 GiB of stable memory per canister. See the [IC resource limits](https://docs.internetcomputer.org/references/resource-limits/).

Use **clustered** when point lookups dominate and keys are fixed-size. Use
**`StableBTreeMap`** when you need ordered iteration / range scans, or variable-size
(`Bound::Unbounded`) keys.

## Usage

```rust
use ic_stable_structures::DefaultMemoryImpl;
use ic_stable_clustered_hash_map::StableClusteredHashMap;

let map = StableClusteredHashMap::<u64, u64, _>::new(DefaultMemoryImpl::default()).unwrap();
map.insert(1, 10).unwrap();
assert_eq!(map.get(&1), Some(10));
assert_eq!(map.remove(&1).unwrap(), Some(10));
assert!(map.is_empty());
```

`DefaultMemoryImpl` is `ic-stable-structures`'s alias: **wasm** canisters use real
stable memory; other targets use an in-memory vector so tests and doctests run on the
host.

Re-open with `init(memory)` after persisting the memory region.

## Constraints

- `K: Storable + PartialEq`, `V: Storable`, both with a **fixed-size** layout
  (`new`/`init` reject non-fixed-size types).
- `M: Memory` (`ic-stable-structures`).
- All mutation uses `&self`; avoid aliasing the same byte range with another mutating
  wrapper while an iterator is alive.

## Benchmark

Run from `crates/ic-stable-clustered-hash-map`:

```bash
canbench            # measure (diff against canbench_results.yml)
canbench --persist  # update the checked-in baseline
```

## Documentation

```bash
cargo doc -p ic-stable-clustered-hash-map --no-deps --open
```
