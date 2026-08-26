# ic-stable-vec-deque

Double-ended **queue** (`VecDeque`) in Internet Computer **stable memory**, V1 **segmented block-ring** layout with magic **`SVD`** and a 128-byte header.

Elements live in fixed-size blocks (`blockSlots` slots each, targeting 256 KiB per block). A directory of consecutive 8-byte entries maps block positions to physical base addresses, and fully drained top-most blocks are recycled through an intrusive free list. Logical index `i` resolves to virtual position `(headOff + i) % virtCap`, then to block `k = r / blockSlots`, slot `k' = r % blockSlots`, byte address `dir[k] + k' · SLOT_SIZE`.

The type is exported as **`VecDeque`** and as **`StableVecDeque`** (alias).

## Features

- `push_front` / `push_back` / `pop_front` / `pop_back` / `get` / `set` in bounded time: at most one element encode/decode plus O(64 bytes) of header writes per operation.
- Growth never relocates elements in bulk. A push into a full deque appends one block page-aligned at the end of stable memory (reusing a drained block from the free list when available), rotates and at most once doubles the directory (`8 · dirSlots` metadata bytes), and migrates at most one block of boundary slots into the new block. No operation scales with the stored length.
- Bounded `Storable` element type `T` (from `ic-stable-structures`).

## Usage

```rust
use ic_stable_structures::DefaultMemoryImpl;
use ic_stable_vec_deque::VecDeque;

let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
dq.push_back(&1).unwrap();
dq.push_front(&0).unwrap();
assert_eq!(dq.to_vec(), vec![0, 1]);
```

`DefaultMemoryImpl` is `ic-stable-structures`’s alias: **wasm** canisters use real stable memory; other targets use an in-memory vector so tests and doctests run on the host.

Re-open with `VecDeque::init(memory)` after `into_memory()`.

## Dependency

- `ic-stable-structures` (workspace version in this repo).

## Documentation

```bash
cargo doc -p ic-stable-vec-deque --no-deps --open
```
