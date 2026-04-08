# ic-stable-bitset

`ic-stable-bitset` is a stable-memory bitset for Internet Computer canisters.
It keeps a heap mirror for reads and uses a durable append-only journal for
`set`, `truncate`, and `remove` mutations.

The primary type is [`BitSet`], also exported as [`StableBitSet`].

## What it stores

The bitset keeps its stable-memory state in a compact header, an append-only
journal, and a packed `u64` snapshot. Journal records are packed `u64` values,
and reopen scans the journal until the first empty slot.

## Operations

- `contains(index)` reads the heap mirror only.
- `set(index, value)` appends a journal record and updates the heap mirror.
- `insert(index)` is a convenience alias for `set(index, true)`.
- `clear(index)` is a convenience alias for `set(index, false)`.
- `remove(index)` removes the bit at `index`, shifts all later bits left by one,
  and appends the remove index to the journal.
- `truncate(len)` shortens the logical length and records the new length.
- `ensure_len(len)` grows the logical length and records the new length.

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

## Example

```rust
use ic_stable_bitset::BitSet;
use ic_stable_structures::DefaultMemoryImpl;

let memory = DefaultMemoryImpl::default();
let bitset = BitSet::new(memory).unwrap();

bitset.insert(7).unwrap();
assert!(bitset.contains(7));
```
