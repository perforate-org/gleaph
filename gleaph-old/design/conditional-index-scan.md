# Conditional Index Scan — Design Document

## Status

- **Phase 1 (equality, single candidate)**: Implemented
- **Phase 2 (multi-candidate selection)**: Implemented
- **Phase 3 (range conditions)**: Implemented

## Background

Prepared statements commonly use optional filter patterns:

```gql
MATCH (u:User)
WHERE ($name IS NULL OR u.name = $name)
  AND ($city IS NULL OR u.city = $city)
RETURN u.name, u.city
```

The planner cannot use indexes for these patterns because **parameter values are unknown at plan time**. `equality_property_predicate()` only detects `Expr::Literal` comparisons and ignores `Expr::Parameter`. This forces a full scan + filter even when an index exists on the property.

## Approach

**Plan template with conditional scan node** — detect optional filter patterns at plan time; at execution time, branch on parameter values to choose between index scan and full scan.

### Design Principles

1. **Minimal runtime overhead** — a single parameter check + branch (IC instruction-budget friendly)
2. **No impact on existing paths** — ConditionalIndexScan is opt-in; regular queries use the existing pipeline unchanged
3. **Multi-candidate** — when multiple optional filters exist, the executor picks the first non-NULL candidate at runtime
4. **Mixed index types** — equality and range candidates can coexist in the same query

## Implementation

### Pattern Detection

`detect_optional_filters()` in `planner.rs` walks the WHERE clause and collects all `$param IS NULL OR var.prop <op> $param` patterns where `<op>` is one of `=`, `>=`, `>`, `<=`, `<`. Both operand orders (within OR and within the comparison) are accepted. The same parameter name must appear in both the IS NULL check and the comparison.

When the operands are reversed (e.g., `$param >= var.prop`), the comparison operator is flipped accordingly (`>=` becomes `<=`, etc.).

For AND-connected expressions, all branches are explored and all matching patterns are returned.

### Plan Structure

```rust
// plan.rs
pub enum ConditionalCmpOp {
    Eq, Ge, Gt, Le, Lt,
}

pub struct ConditionalScanCandidate {
    pub param_name: String,
    pub property: String,
    pub variable: String,
    pub cmp_op: ConditionalCmpOp,
}
```

`ConditionalIndexScan` is emitted when:
1. No literal-based `IndexScan` or `EdgeIndexScan` is applicable
2. At least one optional filter pattern is detected
3. For equality (`Eq`): the property has a registered equality index
4. For range (`Ge/Gt/Le/Lt`): the property has a registered range index
5. The variable is present in the MATCH pattern

Literal `IndexScan` always takes priority over `ConditionalIndexScan`.

### Range Index Infrastructure

#### IndexType::Range

A new `IndexType::Range` variant enables vertex property range indexes. Range indexes use **order-preserving encoding** in the ABP B+ tree so that byte-level key comparison corresponds to value ordering:

| Type | Encoding |
|---|---|
| Int (i64) | XOR sign bit → big-endian |
| Float (f64) | IEEE 754 total-order trick → big-endian |
| Text | Raw UTF-8 (naturally lexicographic) |
| Timestamp (u64) | Big-endian |
| Date (i32) | XOR sign bit → big-endian |
| Time (u64) | Big-endian |
| DateTime | XOR sign bit big-endian i64 + big-endian u32 |
| Duration | XOR sign bit big-endian i32 + XOR sign bit big-endian i64 |

Range index keys use prefix `"IVR"` (vs `"IVE"` for equality).

#### Storage

- **In-memory**: `BTreeMap<(String, Vec<u8>), BTreeSet<u32>>` for ordered traversal during range scans
- **ABP B+ tree**: `scan_range(required_prefix, start_key, end_key)` method for persistent storage
- Both equality and range entries coexist in the same ABP tree (different key prefixes)

### Executor Behavior

`execute_conditional_index_plan_query()` in `executor.rs`:

1. Iterates through candidates in order
2. For each candidate, checks if the parameter is non-NULL **and** the property has the required index type (Equality for `Eq`, Range for `Ge/Gt/Le/Lt`)
3. Routes to `scan_vertices_by_property_eq_auto()` for equality or `scan_vertices_by_property_range_auto()` for range
4. If all parameters are NULL → returns `Ok(None)`, falling back to the generic NodeScan path
5. The `$param IS NULL OR ...` clause short-circuits to TRUE via existing `eval_expr` OR logic

### File Changes

| File | Changes |
|---|---|
| `crates/types/src/lib.rs` | `IndexType::Range` variant, `PlannerStats.range_indexed_vertex_properties` |
| `crates/gql/src/plan.rs` | `ConditionalCmpOp` enum, `cmp_op` field on `ConditionalScanCandidate` |
| `crates/gql/src/planner.rs` | Range pattern detection in `detect_optional_filters()`, range index filtering |
| `crates/gql/src/executor.rs` | `conditional_scan_vertices()` helper, range-aware index type matching |
| `crates/gql/src/stats.rs` | `TableStats.range_indexed_vertex_properties` |
| `crates/pma/src/abp_tree.rs` | `AbpByteKv::scan_range()` method |
| `crates/pma/src/property_store.rs` | `encode_value_ordered()`, `RangeOp`, range index key helpers, `scan_vertices_range()` on `AbpSecondaryEqIndex` |
| `crates/pma/src/pma.rs` | `vertex_prop_range_index` BTreeMap, range index CRUD methods, `scan_vertices_by_property_range[_auto]()`, range backfill/maintenance |
| `crates/graph/src/gql_bridge.rs` | Range index stats population |
| `crates/graph/src/bench/*.rs` | EXPLAIN output shows comparison operator |

### Tests (23 total)

**Pattern detection (9):** basic, reversed OR, reversed eq, nested AND, no-match literal, no-match param mismatch, range >=, range <, reversed range

**Planner (6):** emits ConditionalIndexScan, emits multi-candidate, prefers literal IndexScan, no scan without index, emits range conditional scan, no range scan without range index

**Executor E2E (7+1):** index used with non-NULL param, fallback with NULL param, multi-candidate equality, range >= uses index, range < uses index, range fallback on NULL, mixed equality + range

## Planner Coverage Analysis

### Covered Cases

| Category | Pattern | Notes |
|---|---|---|
| Basic equality | `$p IS NULL OR v.prop = $p` | Requires equality index |
| Basic range | `$p IS NULL OR v.prop >= $p` | Requires range index; all of `>=`, `>`, `<=`, `<` |
| Reversed OR order | `v.prop = $p OR $p IS NULL` | Both OR branch orders accepted |
| Reversed comparison | `$p = v.prop`, `$p >= v.prop` | Operand order flipped; `>=` becomes `<=`, etc. |
| AND-connected filters | `(...) AND (...) AND (...)` | All branches explored, multiple candidates collected |
| Multi-candidate selection | Multiple optional filters | First non-NULL candidate wins at runtime |
| Mixed Eq + Range | `($name IS NULL OR ...) AND ($age IS NULL OR u.age >= $age)` | Eq and Range candidates coexist |
| Literal IndexScan priority | `WHERE u.name = "Alice"` | Literal-based IndexScan always preferred over ConditionalIndexScan |
| NULL fallback | All parameters NULL | Falls back to full NodeScan |
| Mid-chain anchor | `MATCH (a)-[e]->(b:User) WHERE ($p IS NULL OR b.name = $p)` | ConditionalIndexScan on non-first pattern variable |

### Not Covered (Potential Future Work)

| Category | Pattern | Reason |
|---|---|---|
| IS NOT NULL form | `$p IS NOT NULL AND v.prop = $p` | Only `IS NULL OR` pattern detected |
| Not-equal | `$p IS NULL OR v.prop <> $p` | `<>` excluded from conditional operators |
| IN expression | `$p IS NULL OR v.prop IN $list` | Not recognized as optional filter |
| String predicates | `$p IS NULL OR v.prop STARTS WITH $p` | Only comparison operators detected |
| Function-wrapped property | `$p IS NULL OR lower(v.prop) = $p` | Property access must be direct `var.prop` |
| COALESCE | `v.prop = COALESCE($p, v.prop)` | Not recognized as optional filter pattern |
| Compound range (intersection) | `$min IS NULL OR v.age >= $min` AND `$max IS NULL OR v.age <= $max` | Each candidate scans independently; no intersection of two range bounds on same property |
| Edge range indexes | `$p IS NULL OR e.weight >= $p` | Only vertex properties have range indexes |
| Multiple MATCH clauses | Two separate MATCH patterns | ConditionalIndexScan only applies to first MATCH |
| OPTIONAL MATCH / SHORTEST | Optional or path patterns | Not supported as conditional scan anchors |
| Cost-based candidate ordering | N/A | Candidates ordered by detection order (left-to-right), not by selectivity |
| Multi-candidate intersection | N/A | Only first matching candidate used; no intersection of multiple index results |

## Future Work

### Cost Estimation

Integrate into `estimate_cost()`:

```
cost = P(NULL) × full_scan_cost + P(non-NULL) × index_scan_cost
```

P(NULL) defaults to 0.5 (overridable via planner hints).

### Selectivity-Based Candidate Ordering

Currently candidates are ordered by detection order (left-to-right in WHERE clause). A future improvement could order by estimated selectivity from `TableStats.property_selectivity`.

### Edge Range Indexes

Currently only vertex properties support range indexes. Edge range indexes would require extending the edge index infrastructure.
