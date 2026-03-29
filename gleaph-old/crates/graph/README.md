# gleaph-graph

Tenant graph canister for Gleaph.

This crate wraps `gleaph-pma` in an IC canister interface and stores graph data in stable memory.

## Responsibilities

- Initialize or restore PMA graph state
- Expose graph operations as Candid methods
- Persist metadata across canister upgrades
- Bridge IC stable memory APIs to the `gleaph_pma::Memory` trait

## Exposed Candid Methods (current)

Query methods:

- `get_neighbors(vertex_id: nat32) -> vec EdgeInfo`
- `get_stats() -> GraphStats`

Update methods:

- `add_vertex(vertex: VertexData) -> Result<nat64, text>`
- `add_edge(edge: EdgeData) -> Result<nat64, text>`
- `bulk_insert_vertices(vertices: vec VertexData) -> Result<nat64, text>`
- `bulk_insert_edges(edges: vec EdgeData) -> Result<nat64, text>`

The broader planned API surface (including GQL) is documented in `design/`, but not fully implemented yet.

## Upgrade Behavior

- `pre_upgrade`: persists PMA header and canister metadata
- `post_upgrade`: restores PMA state from stable memory header

This crate stores a small graph-canister metadata block in the reserved area of the PMA stable header to preserve `max_vertices` configuration.

## Build

Host build:

```bash
cargo build -p gleaph-graph
```

Wasm build:

```bash
cargo build -p gleaph-graph --target wasm32-unknown-unknown
```

## Benchmarks

The crate has optional `canbench-rs` support.

```bash
cd crates/graph
canbench
```
