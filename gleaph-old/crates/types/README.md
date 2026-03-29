# gleaph-types

Shared types for the Gleaph workspace.

This crate contains:

- Stable-memory layout structs used by the PMA engine
- Candid request/response types shared by canisters and tests
- Common error types/constants

## What Is Included

Stable-memory related structs (`#[repr(C)]`):

- `VertexEntry`
- `EdgeEntry`
- `LogEntry`
- `StableHeader`

API/domain types:

- `VertexData`, `EdgeData`, `EdgeInfo`
- `GraphStats`
- `GraphConfig`, `GraphInfo`
- `AccessLevel`
- `GleaphError`

Constants:

- `STABLE_MAGIC`
- `STABLE_VERSION`

## Design Notes

The struct set follows the repository's PMA/stable-memory design direction (`design/architecture.md`), but this crate intentionally stays lightweight and reusable across:

- the graph canister
- the registry canister
- host-native tests

## Usage

```rust
use gleaph_types::{EdgeData, GraphConfig};

let edge = EdgeData {
    src: 0,
    dst: 1,
    weight: 1.0,
    timestamp: 42,
};

let cfg = GraphConfig {
    name: "demo".into(),
    max_vertices: 1024,
    initial_edge_capacity: 0,
};
```
