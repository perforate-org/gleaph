# Gleaph — Graph Database on the Internet Computer

## Overview

Gleaph is a multi-tenant graph database service for the [Internet Computer](https://internetcomputer.org/) (IC). Each tenant gets an isolated graph canister with its own stable memory. The query language is GQL, based on ISO/IEC 39075:2024.

Primary use cases include e-commerce recommendations (collaborative filtering from purchase history and reviews) and social graphs, but Gleaph works as a general-purpose property graph.

**Status**: Pre-production. No tenants deployed to mainnet yet. API and storage layout may have breaking changes.

## Key Features

### GQL Support

A full GQL engine with a 6-stage pipeline: lexer (nom) → parser → AST → validator → planner → executor (Volcano model).

| Category      | Coverage                                                                                          |
| ------------- | ------------------------------------------------------------------------------------------------- |
| **Read**      | MATCH (multi-hop), WHERE, RETURN, ORDER BY, LIMIT, OFFSET, DISTINCT, OPTIONAL MATCH, WITH, NEXT  |
| **Write**     | INSERT (node/edge), DELETE / DETACH DELETE, SET, REMOVE, MERGE (upsert)                           |
| **Set Ops**   | UNION, UNION ALL, EXCEPT, INTERSECT, OTHERWISE                                                    |
| **Aggregate** | COUNT, SUM, AVG, MIN, MAX, COLLECT, PERCENTILE_CONT/DISC, STRING_AGG; GROUP BY, HAVING           |
| **Paths**     | Variable-length `*min..max`, path variables, SHORTEST / ALL SHORTEST / SHORTEST k                 |
| **Expr/Func** | Arithmetic, comparison, logic, IS NULL, LIKE/ILIKE, CASE, CAST, string/math functions, records    |
| **Planner**   | Cost-based anchor selection, property equality index scan, filter/LIMIT pushdown                   |
| **DDL**       | CREATE/DROP GRAPH, CREATE/DROP GRAPH TYPE, CREATE/DROP SCHEMA, USE GRAPH                          |
| **Params**    | Query parameters via `$param`                                                                     |
| **Resume**    | ContinuationToken-based auto-pagination for large queries and mutations                           |

See [design/gql-specification.md](design/gql-specification.md) for the full reference.

### PMA-CSR Storage Engine

A Packed Memory Array (PMA)-based CSR (Compressed Sparse Row) engine built on IC stable memory. It combines VCSR's vertex-centric PMA with DGAP's log-structured update approach for efficient inserts, rebalancing, and neighborhood scans.

Properties and secondary indexes use an `(a,b)+ tree` — a page-managed B+ tree structure on stable memory.

### Multi-Tenancy

A single registry canister manages the lifecycle of multiple graph canisters. Each tenant has an isolated graph instance with fully separated data.

### Graph Algorithms

Built-in algorithms with IC instruction budget awareness. All support ContinuationToken-based suspend/resume.

- **BFS** — Breadth-first search (shortest path)
- **PageRank** — Node importance scoring (IC certified query support)
- **SSSP** — Single-source shortest path (weighted)
- **Recommend** — Multi-hop collaborative filtering

### ACL (Access Control)

Principal-based three-tier access control.

| Level     | Permissions                                      |
| --------- | ------------------------------------------------ |
| **Read**  | GQL queries, graph stats, algorithm execution    |
| **Write** | Read + INSERT/DELETE/SET mutations               |
| **Admin** | Write + ACL management, index creation, settings |

### Continuation Execution

To handle IC instruction limits, large queries, mutations, and algorithms automatically support suspend/resume.

- **Query**: `query` → if result includes `ContinuationToken` → `query_continue` to fetch the rest
- **Mutation**: `mutate_resumable` → `mutate_continue` to resume interrupted DELETEs
- **Algorithm**: `bfs_resumable` / `compute_pagerank_resumable` / `compute_sssp_resumable`

## Architecture

### System Overview

```
Registry Canister (single)
├── Tenant management & provisioning
├── ACL management
└── Cycle consumption tracking
    │
    ├── Graph Canister (Tenant A) ── stable memory
    ├── Graph Canister (Tenant B) ── stable memory
    └── ...
```

### Crate Dependency Graph

```
types  ──────────────────────────┐
  │                              │
  ├── algo (BFS, PageRank, SSSP) │
  │     │                        │
  ├── pma (PMA storage engine)   │
  │     │                        │
  │     └── gql (GQL engine)     │
  │           │                  │
  │           └── graph (canister)
  │                              │
  └── registry (canister) ───────┘
```

### IC-Agnostic Core

The `pma`, `algo`, and `gql` crates have no IC dependency. The `Memory` trait (stable memory abstraction) and `InstructionBudget` trait (instruction limit abstraction) enable native testing with `VecMemory` and `CountingBudget`. Only the canister crates (`graph`, `registry`) depend on the IC SDK.

See [design/architecture.md](design/architecture.md) for details.

## GQL Examples

### Pattern Matching and Filtering

```sql
MATCH (u:User)-[:Bought]->(p:Product)
WHERE u.name = 'Alice'
RETURN p.name, p.price
ORDER BY p.price DESC
LIMIT 10
```

### Data Insertion

```sql
INSERT (:User {name: 'Bob', age: 30})
```

```sql
MATCH (u:User {name: 'Bob'}), (p:Product {name: 'Widget'})
INSERT (u)-[:Bought {quantity: 2}]->(p)
```

### Aggregation and Path Search

```sql
MATCH (u:User)-[:Bought]->(p:Product)
RETURN p.name, COUNT(u) AS buyers, AVG(u.age) AS avg_age
ORDER BY buyers DESC
```

```sql
MATCH SHORTEST (a:User {name: 'Alice'})-[:Follows*]->(b:User {name: 'Charlie'})
RETURN a, b
```

## Project Structure

| Crate             | Description                                                        |
| ----------------- | ------------------------------------------------------------------ |
| `crates/types`    | Shared types (`#[repr(C)]` stable memory structs, API types, errors) |
| `crates/algo`     | Graph algorithms (IC-agnostic, `GraphView` trait)                  |
| `crates/pma`      | PMA-CSR storage engine (IC-agnostic, `Memory` trait)               |
| `crates/gql`      | GQL engine (lexer → parser → planner → executor)                   |
| `crates/graph`    | Graph canister (IC `cdylib`)                                       |
| `crates/registry` | Registry canister (tenant management)                              |
| `tests`           | Integration tests (unit + PocketIC)                                |
| `design`          | Design documents                                                   |

## Build and Test

### Requirements

- Rust (edition 2024)
- `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`)
- [PocketIC](https://github.com/dfinity/pocketic) runtime (for PocketIC tests)

### Basic Commands

```bash
make build                  # cargo build --workspace
make test                   # cargo test --workspace (unit tests, no IC required)
cargo clippy --workspace    # lint
```

### Single Test

```bash
cargo test -p gleaph-tests test_name
```

### PocketIC Tests (Canister Integration)

```bash
make wasm-e2e-fixtures      # build wasm artifacts first
make test-pocket-ic          # cargo test --workspace -- --ignored --test-threads=1
```

### Benchmarks

```bash
make bench                  # run canbench benchmarks
make bench-persist          # save results to file
```

## API Endpoints

### Graph Canister

**Query calls (no consensus, fast)**

| Endpoint                   | Description                   |
| -------------------------- | ----------------------------- |
| `query(gql)`               | Execute GQL query             |
| `query_resumable(gql)`     | Execute resumable GQL query   |
| `query_continue(token)`    | Continue with token           |
| `get_neighbors(vertex_id)` | List adjacent edges           |
| `get_stats()`              | Graph statistics              |
| `get_planner_stats()`      | Planner stats (selectivity)   |
| `bfs(start, config)`       | Run BFS                       |
| `recommend(config)`        | Run recommendation            |
| `get_canister_info()`      | Canister diagnostics          |
| `get_metrics()`            | Operational metrics           |

**Update calls (consensus required)**

| Endpoint                                                    | Description                          |
| ----------------------------------------------------------- | ------------------------------------ |
| `mutate(gql)`                                               | Execute GQL mutation                 |
| `batch_mutate(gqls)`                                        | Batch mutation                       |
| `mutate_resumable(gql)`                                     | Resumable mutation                   |
| `mutate_continue(token)`                                    | Resume interrupted mutation          |
| `add_vertex(data)` / `add_edge(data)`                       | Programmatic vertex/edge insert      |
| `bulk_insert_vertices(data)` / `bulk_insert_edges(data)`    | Bulk insert                          |
| `create_index(entity, field, type)`                         | Create secondary index               |
| `set_acl_entry(principal, level)`                           | Set ACL entry                        |
| `compute_graph_stats()`                                     | Compute sampling stats for planner   |
| `compute_pagerank(config)` / `compute_sssp(start, config)`  | Run algorithms                       |

### Registry Canister

| Endpoint                              | Description          |
| ------------------------------------- | -------------------- |
| `create_graph(config)`                | Create tenant graph  |
| `delete_graph(id)`                    | Delete tenant graph  |
| `list_graphs()`                       | List graphs          |
| `grant_access(graph_id, principal, level)` | Grant access    |

## Related Documents

| Document                                                              | Content                        |
| --------------------------------------------------------------------- | ------------------------------ |
| [design/architecture.md](design/architecture.md)                      | Architecture details           |
| [design/gql-specification.md](design/gql-specification.md)            | GQL specification reference    |
| [design/gleaph-extensions.md](design/gleaph-extensions.md)            | Non-standard GQL extensions    |
| [design/gql-standard-deviations.md](design/gql-standard-deviations.md) | Standard deviations & status |
| [design/future-roadmap.md](design/future-roadmap.md)                  | Future roadmap                 |
| [docs/README-JA.md](docs/README-JA.md)                               | Japanese README                |
