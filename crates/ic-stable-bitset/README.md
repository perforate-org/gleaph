# ic-stable-bitset

`ic-stable-bitset` is a stable-memory bitset for Internet Computer canisters.
It keeps a heap mirror for reads and uses a durable append-only journal for
`set`, `truncate`, and `remove` mutations.

The primary type is [`Bitset`], also exported as [`StableBitset`].

## What it stores

The bitset keeps its stable-memory state in a compact header, an append-only
journal, and a packed `u64` snapshot. Journal records are packed `u64` values,
and reopen scans the journal until the first empty slot.

## Operations

- `contains(index: u32)` reads the heap mirror only.
- `set(index: u32, value)` appends a journal record and updates the heap mirror.
- `insert(index: u32)` is a convenience alias for `set(index, true)`.
- `clear(index: u32)` is a convenience alias for `set(index, false)`.
- `remove(index: u32)` removes the bit at `index`, shifts all later bits left by one,
  and appends the remove index to the journal.
- `truncate(len: u64)` shortens the logical length and records the new length (bounded by `JOURNAL_LEN_MAX`).
- `ensure_len(len: u64)` grows the logical length and records the new length (same bound).

## Guarantees

- Reads are `O(1)`.
- `set` and `clear` are `O(1)` amortized, with journal append plus a heap
  update.
- `remove(index)` is `O(number of live words after the removed index)` because
  the suffix is shifted left a word at a time after journaling the mutation.
- `truncate(len)` and `ensure_len(len)` are `O(number of live words)` because
  they clear or preserve suffix bits directly.
- Reopening from stable memory replays the mutation journal and reconstructs the
  heap mirror deterministically.

## Notes

- The type is intended for single-writer use.
- The stable memory region should not be mutated through another wrapper while a
  bitset instance is active.
- Mutation records store packed `u64` values and are bounded by the journal
  capacity.
- Bit indices are `u32`; logical length is capped by [`JOURNAL_LEN_MAX`](crate::JOURNAL_LEN_MAX) (`u32::MAX + 1`).

## Example

```rust
use ic_stable_bitset::Bitset;
use ic_stable_structures::DefaultMemoryImpl;

let memory = DefaultMemoryImpl::default();
let bitset = Bitset::new(memory).unwrap();

bitset.insert(7).unwrap();
assert!(bitset.contains(7));
```
