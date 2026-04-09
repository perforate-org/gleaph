# ic-stable-roaring

`ic-stable-roaring` is a stable-memory roaring bitmap for Internet Computer canisters.
It keeps a heap mirror for reads and uses a durable append-only journal for `set`,
`clear`, `ensure_len`, and `truncate` mutations.

The primary type is [`RoaringBitMap`], also exported as [`StableRoaringBitMap`].

## What it stores

The bitmap keeps its stable-memory state in a compact header, an append-only
journal, and a serialized `RoaringTreemap` snapshot. Reopen reads the snapshot
back into memory and replays any pending journal entries.

## Operations

- `contains(index)` reads the heap mirror only.
- `set(index, value)` appends a journal record and updates the heap mirror.
- `insert(index)` is a convenience alias for `set(index, true)`.
- `clear(index)` is a convenience alias for `set(index, false)`.
- `ensure_len(len)` grows the logical length without materializing zero bits.
- `truncate(len)` shortens the logical length and drops set bits at or beyond
  the new end.

## Guarantees

- Reads are `O(1)`.
- `set` and `clear` are `O(1)` amortized, with journal append plus a heap
  update.
- `ensure_len` and `truncate` update the logical length and checkpoint when the
  journal is full.
- Reopening from stable memory restores the roaring snapshot and replays the
  mutation journal deterministically.

## Notes

- The type is intended for single-writer use.
- The stable memory region should not be mutated through another wrapper while a
  bitmap instance is active.
- `len()` returns the logical length, not the cardinality of set bits.
- `remove` is intentionally not part of this API.

## Example

```rust
use ic_stable_roaring::StableRoaringBitMap;
use ic_stable_structures::DefaultMemoryImpl;

let memory = DefaultMemoryImpl::default();
let bitset = StableRoaringBitMap::new(memory).unwrap();

bitset.insert(7).unwrap();
assert!(bitset.contains(7));
```
