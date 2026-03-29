# gleaph-gql-planner

GQL query planner for the Gleaph graph database. Converts parsed GQL ASTs (from `gleaph-gql`) into physical execution plans with cost-based optimization.

## Architecture

```
gleaph-gql (parser/validator) ──> gleaph-gql-planner (this crate)
                                        │
                                        ├── plan.rs      — PhysicalPlan, PlanOp (33 variants), PlanAnnotations
                                        ├── planner.rs   — AST → PhysicalPlan conversion
                                        ├── anchor.rs    — Cost-based anchor (scan start) selection
                                        ├── cost.rs      — Additive cost model + predicate selectivity
                                        ├── pushdown.rs  — Filter/Limit pushdown, EVFusion, TopK, predicate reordering
                                        ├── join_order.rs— Greedy left-deep join ordering + cyclic pattern detection
                                        ├── semantic.rs  — Semantic analysis (property access, narrowing facts)
                                        ├── cse.rs       — Common Subexpression Elimination detection
                                        ├── stats.rs     — GraphStats trait + cost constants + PropertyHistogram
                                        └── explain.rs   — Human-readable plan output
```

## Key Types

- `PhysicalPlan` — Ordered sequence of `PlanOp` + `PlanAnnotations`
- `PlanOp` — 33 operator variants (see below)
- `GraphStats` trait — Optional statistics for cost-based decisions
- `TableStats` — Concrete stats with label cardinality, property selectivity, histograms, schema info

### PlanOp Variants (33)

**Scan:** NodeScan, IndexScan, EdgeIndexScan, ConditionalIndexScan, IndexIntersection
**Filter/Traversal:** PropertyFilter, Filter, Expand, ExpandFilter (EVFusion), ShortestPath, WorstCaseOptimalJoin
**GQL-specific:** Let, For, CallProcedure, InlineProcedureCall, UseGraph
**Join:** HashJoin, CartesianProduct
**Aggregation/Output:** Aggregate, Project, Sort, Limit, TopK, Materialize, SetOperation, OptionalMatch
**DML:** InsertVertex, InsertEdge, SetProperties, RemoveProperties, DeleteVertex, DetachDeleteVertex

## Optimization Passes (9)

1. **Filter pushdown** — Move PropertyFilter to earliest stage where vars available
2. **Predicate reordering** — Sort predicates by selectivity (most selective first)
3. **EVFusion** — Fuse Expand + PropertyFilter on dst → ExpandFilter (GOpt SIGMOD 2025)
4. **FilterIntoPattern** — Fuse dst inline filters at plan time → ExpandFilter
5. **Late project** — Ensure Project after all filters
6. **Limit pushdown** — Move Limit before Project when safe
7. **TopK fusion** — Sort + Limit → TopK heap operator
8. **WCOJ replacement** — Cyclic patterns → WorstCaseOptimalJoin
9. **CSE detection** — Detect common subexpressions (annotation-only)

## Public API

```rust
// Core planning
build_plan(query: &LinearQueryStatement, stats: Option<&dyn GraphStats>) -> PhysicalPlan
build_composite_plan(composite: &CompositeQueryExpr, stats: Option<&dyn GraphStats>) -> PhysicalPlan
build_statement_plan(stmt: &Statement, stats: Option<&dyn GraphStats>) -> PhysicalPlan
build_block_plan(block: &StatementBlock, stats: Option<&dyn GraphStats>) -> PhysicalPlan

// Output
explain_plan(plan: &PhysicalPlan) -> String
```

## Building & Testing

```sh
cargo build
cargo test          # 126 tests
```

Rust edition 2024, MSRV 1.88. Depends on `gleaph-gql` (path dependency at `../gql`).

## Conventions

- Planner operates on `gleaph-gql` AST types directly (no intermediate IR)
- Cost model constants are in `stats.rs` (ported from gleaph-old, extended)
- Anchor selection priority: property-equality > property-range > inline-property > schema-endpoint > label-cardinality > full-scan
- Path patterns use lookahead to resolve edge destination variables
- Optimization passes run in dependency order (pushdown → reorder → fusion → limit → topk → wcoj)
- Adaptive reoptimization hints annotate plans for executor-side re-evaluation
- Executor lives in a separate crate (future `gleaph-gql-executor`)
