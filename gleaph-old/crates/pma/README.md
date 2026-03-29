# gleaph-pma

Packed Memory Array (PMA)-based graph storage engine used by Gleaph.

This crate is the core data-structure implementation and is intentionally IC-agnostic. It uses a `Memory` trait so the same PMA logic can run against:

- `VecMemory` for fast host-native tests
- IC stable memory adapters in the graph canister

## Scope

Implemented in this crate:

- PMA capacity/segment parameter calculation
- Stable-memory region layout helpers
- Per-segment overflow log support
- Edge insertion, rebalance, resize, neighbor collection
- Graph stats and header persistence helpers

Not implemented here:

- IC canister endpoints
- Registry/tenant management
- GQL parsing/execution (future design work in `design/`)

## Public API (high level)

- `PmaGraph<M>`
- `PmaParams`
- `compute_capacity(...)`
- `Memory` trait
- `VecMemory`

Modules:

- `memory`: backend abstraction + in-memory test backend
- `layout`: stable-memory offsets and field encoding/decoding
- `segment_log`: per-segment overflow log utilities
- `pma`: core graph algorithm implementation
- `math`: PMA math helpers

## Example (host-native)

```rust
use gleaph_pma::{PmaGraph, VecMemory};

let mem = VecMemory::default();
let mut g = PmaGraph::new(mem, 16).expect("init");

g.insert(0, 1, 1.0, 100).expect("insert");
g.insert(0, 2, 0.5, 101).expect("insert");

let neighbors = g.collect_neighbors(0).expect("neighbors");
assert_eq!(neighbors.len(), 2);
```

## Why This Crate Is Separate

The `design/phase1-implementation.md` plan calls out a core requirement: PMA logic must be testable outside the Internet Computer runtime. This crate is the implementation of that boundary.
