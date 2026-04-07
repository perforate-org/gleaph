# ic-stable-vec-deque

Double-ended **queue** (`VecDeque`) in Internet Computer **stable memory**, V1 layout with magic **`SVD`**. The 64-byte header matches the prefix of [`ic_stable_structures::vec::Vec`] (`SVC`); extra fields store a ring-buffer `head` and `capacity`. Elements live from byte offset **64**.

The type is exported as **`VecDeque`** and as **`StableVecDeque`** (alias).

## Features

- `push_front` / `push_back` / `pop_front` / `pop_back` / `get` / `set` in **O(1)** amortized time; growth linearizes the ring and may double capacity (**O(len)**).
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

`DefaultMemoryImpl` is `ic-stable-structures`’s alias: **wasm32** canisters use real stable memory; other targets use an in-memory vector so tests and doctests run on the host.

Re-open with `VecDeque::init(memory)` after `into_memory()`.

## Dependency

- `ic-stable-structures` (workspace version in this repo).

## Documentation

```bash
cargo doc -p ic-stable-vec-deque --no-deps --open
```
