# ic-stable-slot-map

Generational **slot map** on Internet Computer **stable memory** (`ic_stable_structures::Memory`). Each allocation returns a **`SlotKey`** `(index, generation)`; removals bump the generation so old keys go stale. A freelist reuses physical slots without shifting storage.

The primary type is **`SlotMap`**. **`StableSlotMap`** is the same type, re-exported as a compatibility alias.

## Features

- **V1 on-disk layout** with magic `SSM`: 64-byte header, then a fixed-width cell per slot (`Storable` `T` must be bounded).
- **O(1)** amortized `insert` / `remove` / `get` / `set`; **O(slot_capacity)** when doubling capacity.
- **`iter_occupied`**: scan `0 .. slot_capacity`, yield `(SlotKey, T)` for occupied cells only.

## Usage

```rust
use ic_stable_slot_map::SlotMap;
use ic_stable_structures::DefaultMemoryImpl;

let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
let key = map.insert(&42).unwrap();
assert_eq!(map.get(key), Some(42));
map.remove(key).unwrap();
assert_eq!(map.get(key), None);
```

Re-open existing memory with `SlotMap::init(memory)`.

## Layout (summary)

| Region       | Content                                                                                        |
| ------------ | ---------------------------------------------------------------------------------------------- |
| Bytes 0–63   | Header: magic `SSM`, version, `live_count`, cell metadata, `slot_capacity`, `free_head`        |
| From byte 64 | Slot cells: each either occupied (`generation` + `T`) or vacant (`generation` + freelist link) |

Details and invariants are documented on `SlotMap` via `cargo doc` (see crate root and `slot_map` module).

## Dependency

- [`ic-stable-structures`](https://docs.rs/ic-stable-structures) (this repo uses the workspace version).

## Documentation

```bash
cargo doc -p ic-stable-slot-map --no-deps --open
```
