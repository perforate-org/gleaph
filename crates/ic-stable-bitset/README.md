# ic-stable-bitset

`ic-stable-bitset` is a stable-memory bitset for Internet Computer canisters.
It keeps a heap mirror for reads and uses a small durable journal in stable memory
so updates can be checkpointed without rewriting the whole snapshot every time.

The primary type is [`BitSet`], also exported as [`StableBitSet`].

## What it stores

The bitset keeps its stable-memory state in a compact header, an append-only
journal, and a packed `u64` snapshot. The journal records pending `set` and
`truncate` operations. Each packed record uses a payload that fits in `2^61`
values, so the maximum representable bit index or logical length is `2^61 - 1`.

## Operations

- `contains(index)` reads the heap mirror only.
- `set(index, value)` appends a journal record, updates the heap mirror, and
  checkpoints when the journal fills.
- `insert(index)` is a convenience alias for `set(index, true)`.
- `remove(index)` is a convenience alias for `set(index, false)`.
- `truncate(len)` shortens the logical length and records the shrink in the
  journal.
- `ensure_len(len)` grows the logical length without setting any bits.

## Guarantees

- Reads are `O(1)`.
- Updates are `O(1)` amortized, with checkpointing proportional to the number of
  live words.
- Reopening from stable memory replays the journal and reconstructs the heap
  mirror deterministically.

## Notes

- The type is intended for single-writer use.
- The stable memory region should not be mutated through another wrapper while a
  bitset instance is active.
- The maximum logical index and length supported by the packed journal is
  `2^61 - 1`.

## Example

```rust
use ic_stable_bitset::BitSet;
use ic_stable_structures::DefaultMemoryImpl;

let memory = DefaultMemoryImpl::default();
let bitset = BitSet::new(memory).unwrap();

bitset.insert(7).unwrap();
assert!(bitset.contains(7));
```
