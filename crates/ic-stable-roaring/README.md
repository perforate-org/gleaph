# ic-stable-roaring

`ic-stable-roaring` is a stable-memory roaring bitmap for Internet Computer canisters.
It keeps a heap mirror for reads and uses a durable append-only journal for `set`,
`clear`, `ensure_len`, and `truncate` mutations.

The primary type is [`RoaringBitmap`], also exported as [`StableRoaringBitmap`].

## What it stores

The bitmap keeps its stable-memory state in a compact header, an append-only
journal, and a serialized [`RoaringBitmap`](https://docs.rs/roaring/latest/roaring/bitmap/struct.RoaringBitmap.html) snapshot. Reopen reads the snapshot
back into memory and replays any pending journal entries. Bit indices are `u32`;
the layout header version byte is unchanged from earlier releases, but the snapshot
bytes are **not** compatible with older `RoaringTreemap` snapshots—reopen may fail
with an invalid layout error until memory is reinitialized.

## Operations

- `contains(index: u32)` reads the heap mirror only.
- `set(index: u32, value)` appends a journal record and updates the heap mirror.
- `insert(index: u32)` is a convenience alias for `set(index, true)`.
- `clear(index: u32)` is a convenience alias for `set(index, false)`.
- `ensure_len(len: u64)` grows the logical length without materializing zero bits (bounded by [`JOURNAL_LEN_MAX`](crate::JOURNAL_LEN_MAX)).
- `truncate(len: u64)` shortens the logical length and drops set bits at or beyond
  the new end (same bound).

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
- Logical length is capped by [`JOURNAL_LEN_MAX`](crate::JOURNAL_LEN_MAX) (`u32::MAX + 1`).

## Example

```rust
use ic_stable_roaring::StableRoaringBitmap;
use ic_stable_structures::DefaultMemoryImpl;

let memory = DefaultMemoryImpl::default();
let bitset = StableRoaringBitmap::new(memory).unwrap();

bitset.insert(7).unwrap();
assert!(bitset.contains(7));
```
