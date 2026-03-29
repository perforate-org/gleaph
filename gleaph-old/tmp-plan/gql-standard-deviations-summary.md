# GQL Standard Deviations — Gleaph Implementation

Catalog of differences between Gleaph's GQL implementation and the
ISO/IEC 39075 GQL standard.

---

## Fundamental Constraints

### F1. Directed-Only Graph

GQL defines undirected edges via `~[e:L]~`. Gleaph is a **directed-only**
graph database; `~` syntax produces a parse error. Bidirectional matching
(`-[e:L]-`, `-/L/-`) traverses directed edges in both directions — this is
semantically different from true undirected edges. `IS DIRECTED` always
returns `true`.

### F2. No Session Management (IC-Incompatible)

GQL §7 (`SESSION SET GRAPH/SCHEMA/TIME ZONE/PARAMETER`, `SESSION RESET`,
`SESSION CLOSE`) is not implemented and not planned. The IC execution model
is stateless per call.

### F3. No Transaction Management (IC-Incompatible)

GQL §8 (`START TRANSACTION`, `COMMIT`, `ROLLBACK`) is not implemented and
not planned. Each IC update call is atomic.

---

## Syntax & Semantics Gaps

### S1. YIELD Projection Not Enforced

`NEXT YIELD col1, col2` is parsed but the column projection is not applied.
Both `YIELD *` and named columns pass through all bindings. Filtering relies
on the subsequent `RETURN` clause.

### S2. BFS Label-Expression Filtering on Variable-Length Paths

When a variable-length path uses a label expression (e.g.
`[e:A|B *1..5]`), BFS falls back to `edge_label: None` and traverses all
edges. Post-filtering ensures correctness but hurts performance.

**Resolution plan:** It is now acceptable to add a dependency from the
`algo` crate to `gql` (or to extract a small shared trait/enum into
`types`) so that BFS can receive and evaluate `LabelExpr` during traversal.
This removes the architectural constraint that originally caused this
limitation.

### S3. SHORTEST GROUP Requires a Path Variable

GQL allows `SHORTEST GROUP` without a path variable. Gleaph requires one:

```sql
-- OK
MATCH SHORTEST GROUP p = (a)-[*]->(b) RETURN p
-- Error
MATCH SHORTEST GROUP (a)-[*]->(b) RETURN a, b
```

### S4. Parenthesized Sub-Path Patterns (T2.7) — Not Implemented

GQL supports `(a)(sub = -[:KNOWS]->(x) WHERE x.age > 20){1,3}(b)`.
Not parsed or executed. Requires significant parser grammar changes;
low priority.

### S5. KEEP Clause — Not Implemented

GQL §16.4 `KEEP` clause on path patterns/variables is not supported.

---

## Type System & Value Gaps

### V1. No Temporal Types

GQL §21.2 defines DATE, DATETIME, DURATION literal types. Gleaph uses
`Value::Timestamp(u64)` (nanoseconds since epoch) — a plain integer, not a
structured temporal type.

### V2. CURRENT_TIMESTAMP Returns 0 on wasm32

On IC canisters, `CURRENT_TIMESTAMP` returns `0` and `CURRENT_DATE`
returns `"1970-01-01"` because `ic_cdk::api::time()` is not injected from
the graph bridge. Callers should supply time via query parameters.

### V3. No Byte String Type

GQL §18.9 `BYTES` / `BINARY` / `VARBINARY` types are not supported.

### V4. No Full Static Type System (T6.7)

Typed declarations (`:: STRING NOT NULL`), type unions (`INT | FLOAT`),
and property type constraints in graph type definitions are not
implemented. Values remain dynamically typed via the `Value` enum.

---

## DDL & Graph Management Gaps

### D1. USE GRAPH — No Transparent Routing

`USE GRAPH name` is parsed and returns `graph_name` / `canister_id`
columns, but does not perform transparent cross-canister routing within the
GQL pipeline. Routing is handled by dedicated `query_via` / `mutate_via`
endpoints.

### D2. CREATE/DROP GRAPH — Not Executable as GQL Statements

`CREATE GRAPH` / `DROP GRAPH` are no-ops at the GQL execution layer.
Actual lifecycle management goes through IC registry endpoints
(`create_graph_remote` / `drop_graph_remote`).

### D3. GRAPH TYPE / SCHEMA — Partial

Label definitions and label enforcement on mutations exist. Property type
constraints and edge type definitions are deferred. `CREATE/DROP SCHEMA`
is not connected to IC namespace management.

---

## Gleaph-Specific Extensions (Non-Portable)

Features present in Gleaph that have no GQL equivalent:

| Extension | Origin | Notes |
|---|---|---|
| `MERGE ... ON CREATE SET / ON MATCH SET` | Cypher | Node-only upsert |
| `STARTS WITH` / `ENDS WITH` / `CONTAINS` | Cypher | GQL uses `LIKE` only |
| `ILIKE` | PostgreSQL | Case-insensitive `LIKE` |
| `FOR item IN list` | Gleaph | List iteration statement |
| `FINISH` | Gleaph | Execute without returning results |
| `FILTER` | Gleaph | Streaming row filter |
| `LET x = e IN body END` | Gleaph | Inline let expression |
| `VALUE { subquery }` | Gleaph | Scalar subquery |
| `0xFF` / `0o77` / `0b1010` / `1.5e3` | Gleaph | Non-standard numeric literals |
| `gleaph_weight(e)` / `gleaph_timestamp(e)` | Gleaph | Structural edge attributes |
| `PERCENTILE_CONT/DISC` / `STRING_AGG` | SQL | SQL:2003/2016 aggregates |

---

## Resolved Deviations (Historical)

| Former Deviation | Fix |
|---|---|
| `CREATE` for data insertion | Replaced by `INSERT` |
| `SKIP n` | Replaced by `OFFSET n` |
| `"str"` as string literal | `'str'` is string; `"ident"` is identifier |
| Only `OPTIONAL MATCH` | `OPTIONAL { MATCH }` also accepted |
| Only `WITH` for piping | `NEXT YIELD` also accepted |

---

## Planner Gaps

### P1. Edge Property Hints Not Index-Pushed

`[e:TYPE {prop: val}]` is parsed and evaluated, but property predicates
are not pushed into `PropertyFilter` in the planner for index scans.
Filtering is post-traversal only.

### P2. Cost Model Not Calibrated (T0.4)

Cost constants (`C_scan`, `C_expand`, `C_filter`, `C_sort`) are rough
estimates, not calibrated against actual IC instruction counts.
