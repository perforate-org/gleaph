# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
make build                  # cargo build --workspace
make test                   # cargo test --workspace (unit tests, no IC required)
make test-pocket-ic         # cargo test --workspace -- --ignored --test-threads=1 (requires wasm)
make wasm                   # build graph + registry canisters for wasm32-unknown-unknown
make wasm-e2e-fixtures      # build all wasm including legacy fixture (needed before pocket-ic tests)
cargo clippy --workspace    # lint (no Makefile target exists)
```

Run a single test:

```bash
cargo test -p gleaph-tests test_name
cargo test -p gleaph-tests -- --ignored test_name  # for #[ignore] PocketIC tests
```

Benchmarks: `make bench` / `make bench-persist` (runs canbench in crates/graph).

## Architecture

Gleaph is a multi-tenant graph database for the Internet Computer (IC). Each tenant gets an isolated graph canister; a single registry canister manages lifecycle and ACLs.

### Crate Dependency Graph

```
types  ──────────────────────────┐
  │                                               │
  ├── algo (BFS, PageRank, SSSP, recommend)      │
  │     │                                        │
  ├── pma (PMA storage engine)                   │
  │     │                                        │
  │     └── gql (query language engine)         │
  │           │                                  │
  │           └── graph (IC graph canister)     │
  │                                               |
  └── registry (IC registry canister) ──────┘
```

- **types** — Shared `#[repr(C)]` stable memory structs (VertexEntry 16B, EdgeEntry 16B, StableHeader 4096B), API types, `GleaphError`, GQL value types. Compile-time size assertions enforce layout stability.
- **algo** — IC-agnostic graph algorithms. Defines `GraphView` trait (the read interface for the graph). `InstructionBudget` trait guards against runaway computations (with `IcBudget` for canister use, `CountingBudget`/`UnlimitedBudget` for tests).
- **pma** — PMA-CSR storage engine (derived from VCSR + DGAP papers). Implements `GraphView`. Uses a `Memory` trait (`VecMemory` for native tests, `IcStableMemory` for canister). Key subsystems: segment logs (write buffering), label index, property store (dual-mode: append-log or (a,b)+ tree with migration support), abp_tree (page-based B+ tree for stable memory).
- **gql** — Full GQL pipeline: `lexer` (nom) → `parser` → `AST` → `validate` → `planner` (cost-based, anchor selection, filter pushdown) → `executor` (volcano-model with `RowIterator`). Supports MATCH/CREATE/DELETE/SET/REMOVE, aggregation, UNION/EXCEPT/INTERSECT, OPTIONAL MATCH, SHORTEST path, WITH clauses.
- **graph** — IC graph canister (`cdylib`). Endpoints split into `#[query]` (reads) and `#[update]` (mutations). Thread-local state wraps `PmaGraph<IcStableMemory>`. GQL bridge applies guardrails (max query length 16KB, max rows, execution step limits). Supports IC certified queries via RbTree + rkyv-cached results.
- **registry** — IC registry canister. Manages tenant graph canisters. Handles legacy snapshot upgrade compatibility.

### Stable Memory Layout

```
Header (4KB) → Vertex Array → Edge Array/PMA → Segment Tree → Segment Log Area →
Segment Log Index → Property Store → Secondary Indexes
```

All layout offsets computed in `pma/layout.rs`. The header's `_reserved` bytes store overlay metadata persisted across canister upgrades.

### GQL Pipeline

```
string → lexer::tokenize → parser::parse_statement → AST → validate → planner::build_plan → executor::execute_plan → QueryResult
```

The planner uses cost-based anchor selection (property-equality index > label-only > full-scan) and greedy left-deep join ordering.

### Key Design Patterns

- **IC-agnostic core**: `pma`, `algo`, and `gql` crates have no IC dependency. The `Memory` trait and `InstructionBudget` trait allow native testing with `VecMemory` and `CountingBudget`.
- **Dual compilation**: Canister crates (`graph`, `registry`) are `cdylib + rlib`. Some dependencies are `cfg(target_arch = "wasm32")`-gated.
- **Canister upgrades**: `pre_upgrade`/`post_upgrade` with versioned header and overlay snapshots. Backward compat handled via fallback decode paths.

## Testing Strategy

- **Unit tests** run natively using `VecMemory` — no IC infrastructure needed. Located in `tests/src/` and inline in crate modules.
- **PocketIC tests** are `#[ignore]`-tagged and require wasm artifacts built first (`make wasm-e2e-fixtures`). Must run single-threaded (`--test-threads=1`).
- **Upgrade compat tests** use a legacy registry fixture in `tests/fixtures/legacy-registry/`.

## Tech Stack

Rust edition 2024, IC SDK (ic-cdk 0.19), nom 8 (parsing), rapidhash (hashing), rkyv (zero-copy serialization for cached results), serde_cbor (overlay snapshots), PocketIC 12 (integration tests), canbench (benchmarks), `icp` CLI for deployment.
