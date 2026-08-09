# ic-stable-clustered-hash-map

`StableClusteredHashMap` — a hash map in Internet Computer stable memory, using
clustered hashing (Amble & Knuth 1974, "Ordered Hash Tables") for compact storage and
fast point lookups.

## Operations

- `get` / `insert` / `remove` / `contains_key` in **O(1)** amortized time (by key).
- Growing when `len >= 3/4 * buckets` rehashes **incrementally**: the remap is spread
  across subsequent operations (amortized **O(1)** per op), so there is no
  stop-the-world rehash.
- Iteration is in unordered slot order (`iter`, and `iter_from` for resumable bounded
  scans).

## Compared to `StableBTreeMap`

`StableBTreeMap` (ic-stable-structures) is the ordered-tree baseline that motivated this
crate. Measured with canbench (`N = 4096`, per-op instructions):

| operation | clustered | btree   | clustered vs btree |
| --------- | --------- | ------- | ------------------ |
| get       | ≈ 4.2k    | ≈ 26.6k | ~6.3x faster       |
| insert    | ≈ 39k     | ≈ 60.3k | ~1.5x faster       |
| remove    | ≈ 9.4k    | ≈ 57.4k | ~6.1x faster       |

Use **clustered** when point lookups dominate and keys are fixed-size. Use
**`StableBTreeMap`** when you need ordered iteration / range scans, or variable-size
(`Bound::Unbounded`) keys.

## Usage

```rust
use ic_stable_structures::DefaultMemoryImpl;
use ic_stable_clustered_hash_map::StableClusteredHashMap;

let map = StableClusteredHashMap::<u64, u64, _>::new(DefaultMemoryImpl::default()).unwrap();
map.insert(1, 10);
assert_eq!(map.get(&1), Some(10));
assert_eq!(map.remove(&1), Some(10));
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
