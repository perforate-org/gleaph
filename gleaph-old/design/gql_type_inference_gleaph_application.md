# Applying `gql_type_inference_design.md` to Gleaph

## Status

Partially implemented. This document started as a proposed implementation plan,
but a substantial subset is now in the codebase.

Current high-level status:

- `done`:
  - `semantic.rs` exists and is wired into `type_check`
  - structural semantic analysis exists
  - expression/boolean constraint collection exists
  - planner consumes semantic property/aggregate facts
  - stable explain output exists in planner
  - graph API, canister query endpoint, JS SDK, and CLI can call explain
  - flow-sensitive narrowing from WHERE predicates (`IS NOT NULL`, `IS LABELED`, `type(e) = 'X'`)
  - OPTIONAL MATCH null lifting (property access on optional vars strips NonNull)
  - `Type::Never` for contradiction detection and propagation
  - narrowing fact extraction and planner-facing export
  - constraint-based type checking is primary path (legacy dead code removed)
  - explicit constraint solving via `TypedConstraint` + `SolvedTypeTable`
  - semantic-driven anchor selection (equality/range/inline/optional-filter predicates)
  - direction-aware endpoint contradiction detection (incoming, undirected, edge-narrowing, multi-hop)
  - semantic capture of OR-connected optional filter patterns (`$param IS NULL OR ...`)
  - semantic capture of inline node WHERE predicates
- `partial`:
  - planner still uses some AST-local heuristics as fallback alongside semantic facts
  - full `SemanticType` enum not yet introduced (using existing `Type` + `Type::Never`)
- `done` (newly completed):
  - diagnostic provenance tracking (WarningProvenance on TypeWarning)
  - path-shape semantic typing (PathTypeInfo with min/max hops)
  - schema-endpoint-driven multi-hop join order (greedy_chain_score)

### Current Completion Snapshot

Status by major area, with the primary implementation anchor for each:

- `done`: semantic skeleton and reusable semantic artifact
  - `crates/gql/src/semantic.rs`
- `done`: `type_check` is constraint-based, legacy dead code removed
  - `crates/gql/src/type_check.rs`
- `done`: planner consumes semantic facts (equality/range/inline/optional-filter predicates, type diagnostics, contradiction flag)
  - `crates/gql/src/planner.rs`
  - `crates/gql/src/plan.rs`
- `done`: graph type endpoint contradictions checked at query time (direction-aware, edge-narrowing, multi-hop)
  - `crates/gql/src/type_check.rs`
  - `crates/graph/src/gql_bridge.rs`
- `done`: impossible-pattern reasoning surfaced in explain, query fast rejection, and prepared-query metadata
  - `crates/graph/src/gql_bridge.rs`
  - `crates/types/src/lib.rs`
- `done`: client surfacing exists for explain and prepared metadata
  - `crates/cli/src/main.rs`
  - `sdk/js/src/types.ts`
  - `sdk/js/src/idl.ts`

Remaining gaps:

- `partial`: planner still uses AST-walk fallbacks alongside semantic facts for some edge cases
- `partial`: endpoint/type contradiction reasoning for complex path shapes and unions
- `future`: full `SemanticType` enum replacement (cosmetic rename)

This document translates the target architecture in
`design/gql_type_inference_design.md` into Gleaph-specific implementation terms.
It is not a clean-sheet design. Gleaph already has a partial static type system,
schema-aware property inference, strict/warning modes, and graph type metadata.

The main question is therefore:

> How should Gleaph evolve from its current `validate + type_check` pipeline into a
> pattern-first, constraint-based semantic analysis pipeline without breaking the
> current engine shape?

## 1. Current Gleaph State

Gleaph already implements a lightweight version of the proposed design:

- Pattern-first variable binding:
  - `MATCH` binds node vars as `Type::Node(labels)` and edge vars as `Type::Edge(label)`.
- Gradual typing:
  - `Type::Unknown` suppresses warnings and preserves compatibility under open schema.
- Schema-aware property inference:
  - `PropertySchema` feeds node/edge property types into static checking.
- Nullability refinement:
  - required properties are represented as `Type::NonNull(inner)`.
- Boundary typing:
  - `WITH` projection and `NEXT YIELD` propagate inferred output bindings.
- Strictness modes:
  - warning mode and strict mode are already implemented.

In other words, Gleaph already has the beginning of:

- pattern-first typing
- schema-aware refinement
- gradual typing
- boundary typing

The explicit constraint engine (`TypedConstraint` + `SolvedTypeTable` + `ConstraintGenerator`) is now the primary type-checking path.

## 2. Current Architecture Mapping

The current semantic pipeline is roughly:

1. Parse AST
2. `validate_statement()`:
   - feature gating
   - scope/name checks
   - clause ordering checks
3. `type_check_statement[_with_schema|_strict]()`:
   - build `TypeEnv`
   - infer expression types on demand
   - emit warnings/errors immediately
4. planner/executor run mostly independently from typing facts

This maps to the target design as follows.

### Already aligned

- Parsing / AST construction: `done`
- Name resolution / scope construction: `partial` and still centered in `validate`
- Pattern typing pass: `partial`, now split between semantic structural analysis and legacy env building
- Expression typing: `partial`, semantic constraints exist but final typing is not fully semantic-driven
- Schema refinement: `partial`, implemented for property lookup and simple endpoint/type contradiction analysis
- Boundary typing: `done` for structural row schema extraction, `partial` for full semantic typing use

### Missing or incomplete

- explicit constraint generation objects: `done`
- explicit constraint solving phase: `done`
- provenance per inferred fact / warning: `future`
- planner-facing semantic fact export: `done`
- flow-sensitive narrowing from predicates: `done`
- endpoint-aware typing from edge type schema: `done`
- `OPTIONAL MATCH` nullability lifting: `done`
- typed grouping boundary model: `done`
- contradiction detection / `Never`: `done`

## 3. What “Apply It to Gleaph” Means

For Gleaph, applying the design should mean:

1. Preserve the current user-visible behavior:
   - open schema remains permissive
   - `Unknown` remains first-class
   - warning mode stays non-breaking
2. Replace ad hoc on-demand inference with a reusable semantic analysis product.
3. Feed graph type metadata back into query-time reasoning, not only mutation-time enforcement.
4. Make semantic facts available to:
   - diagnostics
   - planner rewrites
   - future IDE/LSP support

It should not mean:

- introducing HM-style global inference
- requiring full schema for all graphs
- making strict mode the default

## 4. Recommended Gleaph Semantic Pipeline

Recommended pipeline:

1. Parse AST
2. Scope validation
3. Pattern skeleton pass
4. Constraint generation pass
5. Schema refinement pass
6. Constraint solving pass
7. Boundary materialization pass
8. Semantic fact export
9. Planner / diagnostics consume the result

Implementation status against this pipeline:

1. Parse AST: `done`
2. Scope validation: `done`
3. Pattern skeleton pass: `done`
4. Constraint generation pass: `done`
5. Schema refinement pass: `done` (property lookup + endpoint contradiction + edge narrowing)
6. Constraint solving pass: `done`
7. Boundary materialization pass: `done` (row schemas + aggregation boundary validation)
8. Semantic fact export: `done` (equality/range/inline/optional-filter predicates, narrowing facts)
9. Planner / diagnostics consume the result: `done` (semantic anchor selection, type diagnostics, contradiction flag)

### 4.1 Pattern Skeleton Pass

Replace the current `build_env_from_match_clause()` role with a richer binding skeleton.

Each bound variable should receive an initial symbolic kind:

- `BindingKind::Node`
- `BindingKind::Edge`
- `BindingKind::Path`
- `BindingKind::Scalar`
- `BindingKind::Record`

And associated pattern facts:

- node labels or type annotation
- edge label or type annotation
- edge direction
- variable-length bounds
- path variable linkage

This phase should not try to fully type expressions. Its job is to create typed placeholders.

Status: `done`

Notes:

- `BindingKind`, `BindingInfo`, nullable bindings, path bindings, and row-boundary extraction
  exist in `semantic.rs`.
- `TypeEnv` still builds from match clauses but consumes semantic analysis for narrowing and constraint facts.

### 4.2 Constraint Generation Pass

Walk expressions and emit typed obligations instead of checking immediately.

Suggested minimal constraint set for Gleaph:

- `HasKind(var, Node|Edge|Path|Scalar|Record)`
- `HasScalarType(expr_id, ValueType)`
- `Comparable(lhs, rhs)`
- `Arithmetic(lhs, rhs, op)`
- `Boolean(expr_id)`
- `PropertyReadable(target, property)`
- `PropertyType(target, property, expected_or_fresh)`
- `Nullable(expr_id, bool_or_unknown)`
- `ProjectsBoundary(boundary_id, column, type)`
- `AggregatesBoundary(boundary_id, group_key_types, aggregate_types)`
- `EdgeEndpoints(edge_var, src_constraints, dst_constraints)`

Soft constraints should remain warnings in permissive mode.
Hard constraints should become strict-mode errors.

Status: `done`

Implemented:

- boolean-context constraints
- arithmetic/comparison constraints
- function-call constraints
- null-test constraints
- subquery constraints
- property-access constraints
- aggregate-call constraints
- WHERE equality/range predicates
- inline node property hints
- optional filter predicates (`$param IS NULL OR var.prop <op> $param`)
- inline node WHERE predicates
- first-class solver variables: `TypeVarId` + `SolvedTypeTable`
- `ConstraintGenerator` owns all type inference via `infer_type()`, `infer_binary_op()`, `infer_fn_call()`, `infer_return_cols()`

### 4.3 Schema Refinement Pass

This is where Gleaph should use active graph type metadata much more aggressively.

Current usage:

- property lookup by labels or edge label

Additional use recommended:

- resolve node type annotations to label/property facts
- resolve edge type annotations to:
  - edge label
  - source label constraints
  - destination label constraints
  - edge property schema
- detect impossible endpoint combinations
- refine property existence:
  - declared required property
  - declared optional property
  - undeclared property under open schema

This requires query-time access to full graph type metadata, not only the reduced
`PropertySchema` view.

Status: `partial`

Implemented today:

- schema-aware property lookup
- simple query-time endpoint contradiction detection for labeled and simple typed edge patterns
- graph-facing explain/planner paths can consume planner stats and semantic property facts

Not yet implemented:

- full query-time use of graph type endpoint metadata across all pattern shapes
- contradiction detection for complex typed edge/path patterns
- richer node/edge type refinement beyond property schema

## 5. Recommended Type Model Changes

The current `Type` enum is sufficient for warning-mode inference, but too small for
the target architecture.

Recommended direction:

```rust
pub enum SemanticType {
    Scalar(ValueType),
    List(Box<SemanticType>),
    Record(RowType),
    Node(NodeTypeInfo),
    Edge(EdgeTypeInfo),
    Path(PathTypeInfo),
    Union(Vec<SemanticType>),
    Unknown,
    Never,
    Nullable(Box<SemanticType>),
}
```

Important notes for Gleaph:

- keep `Unknown`
- add `Never`
- replace ad hoc `NonNull` wrapping with general nullability metadata
- keep row/record typing explicit because `WITH` and `RETURN` are row boundaries

This does not have to replace the current `Type` immediately. A practical migration is:

1. keep `Type` for external behavior
2. introduce internal `SemanticType`
3. adapt diagnostics from `SemanticType`
4. remove old `Type` when planner/executor integration is complete

Status: `done` (enriched type info structs, not full rename)

Notes:

- `Type` enum now uses enriched info structs: `NodeTypeInfo`, `EdgeTypeInfo`, `PathTypeInfo`.
- `NodeTypeInfo`: labels + schema-known property types `(name, ValueType, required)`.
- `EdgeTypeInfo`: label + endpoint constraints `(from_labels, to_labels)` + property types.
- `PathTypeInfo`: optional min/max hop bounds.
- Schema metadata populated at binding time in `build_env_from_bindings()`.
- Property access on nodes/edges uses stored property info first, falls back to schema query.
- `Type::Never` remains the bottom type with full propagation semantics.
- Full `SemanticType` rename deferred (cosmetic, 183 reference points, no functional gain).

## 6. Key Gleaph-Specific Gaps

### 6.1 `OPTIONAL MATCH`

The current design should be applied by null-lifting bindings introduced by optional segments.

Examples:

- `OPTIONAL MATCH (a)-[:X]->(b)` should make `a` and `b` nullable if they are first introduced there
- property access from optionally bound vars should carry nullable output

This is a high-value missing piece because Gleaph already supports `OPTIONAL MATCH` at execution time.

Status: `done`

Notes:

- optional bindings are represented structurally in `BindingInfo.nullable`
- `TypeEnv.optional_vars` tracks which variables come from OPTIONAL MATCH
- property access on optional vars strips `NonNull` (schema NOT NULL properties become
  nullable because the binding itself might be null)
- 16 new tests covering OPTIONAL MATCH null lifting in `tests/src/gql_type_check.rs`

### 6.2 Graph Type Endpoints

Gleaph already stores edge endpoint constraints in graph type metadata.
The type system should use those constraints during query analysis.

Example:

- if `LIKES` is declared as `(:User)-[:LIKES]->(:Post)`
- then `MATCH (m:Movie)-[:LIKES]->(n:Movie)` is statically contradictory under strict schema mode

Today this knowledge is used at query time. Direction-aware endpoint
contradiction detection covers outgoing, incoming, and undirected edge
patterns. Edge label narrowing from `type(e) = 'X'` in WHERE propagates
endpoint constraints. Multi-hop chains are checked hop-by-hop. Edge type
annotations resolve to endpoint constraints via the schema.

Status: `done`

### 6.3 Flow-Sensitive Narrowing

Current `WHERE` checking only verifies boolean-ness or mismatches.
It should also refine bindings inside the same query segment.

Examples:

- `n.age IS NOT NULL` narrows `n.age` to non-null for later expressions in the segment
- `n IS LABELED Person` narrows node label set
- `type(e) = 'KNOWS'` can narrow edge label in permissive mode

This can start with a small rule set and does not need full theorem proving.

Status: `done`

Implementation:

- `NarrowingFact` enum in `semantic.rs`: `PropertyNonNull`, `LabelNarrowed`, `EdgeLabelNarrowed`
- `extract_narrowing_facts()` walks AND-connected WHERE predicates (conservative for OR)
- `TypeEnv.apply_narrowing()` applies facts: updates `narrowed_nonnull` set, refines
  node labels and edge labels in bindings
- `infer_expr` for PropertyAccess checks `narrowed_nonnull` to wrap results in `NonNull`
- Label narrowing enables schema property lookup for initially unlabeled nodes
- Edge label narrowing from `type(e) = 'X'` enables edge property schema lookup
- Narrowing facts exported to planner via `PlanAnnotations.narrowing_facts`
- Stable explain output: `semantic-narrowing=n.age:nonnull,n:label(Person)`

### 6.4 Aggregation Boundaries

`WITH` and `RETURN` with aggregates should produce an explicit row schema.

Needed outcomes:

- know which expressions are grouping keys
- know which columns are aggregate outputs
- validate `HAVING` and `ORDER BY` against projected row schema
- export typed aggregate columns to planner

Status: `done`

Notes:

- row schemas for `WITH` / `RETURN` / `NEXT` exist
- aggregate presence and aggregate facts are exported to planner
- `check_aggregation_boundary()` validates that non-aggregate RETURN items are grouping keys when explicit GROUP BY is present
- `GroupingViolation` warning kind propagated through full stack (types, gql, graph bridge, CLI, JS SDK)

### 6.5 Path Typing

Gleaph already has:

- path variables
- path constructors
- variable-length edges
- parenthesized subpaths

The type system should at minimum track:

- value is a path
- optional bound length range
- optional endpoint constraints

Detailed path-shape typing can remain future work.

Status: `partial`

Notes:

- path bindings and `Type::Path` propagation exist
- detailed path-shape / endpoint semantic typing does not

## 7. Recommended Internal Data Structures

Suggested internal result object:

```rust
pub struct SemanticAnalysis {
    pub bindings: BindingTable,
    pub constraints: Vec<Constraint>,
    pub solved_types: SolvedTypeTable,
    pub row_schemas: Vec<RowSchema>,
    pub diagnostics: Vec<SemanticDiagnostic>,
    pub facts: SemanticFacts,
}
```

Suggested supporting structures:

```rust
pub struct BindingInfo {
    pub name: String,
    pub kind: BindingKind,
    pub declared_at: AstNodeId,
    pub current_type: SemanticType,
}

pub struct RowSchema {
    pub boundary_id: BoundaryId,
    pub columns: Vec<(String, SemanticType)>,
}

pub enum Constraint {
    Boolean { expr: ExprId },
    Comparable { lhs: ExprId, rhs: ExprId },
    Arithmetic { lhs: ExprId, rhs: ExprId, op: BinaryOp },
    PropertyType { target: ExprId, property: String, result: TypeVarId },
    EdgeEndpoints {
        edge: BindingId,
        src_labels: Vec<String>,
        dst_labels: Vec<String>,
    },
}
```

The key point is not the exact shape. The key point is to stop encoding all semantics
inside recursive `infer_expr()` calls and to produce a reusable semantic artifact.

## 8. Planner Integration for Gleaph

This is where the design becomes more than diagnostics.

Recommended planner-facing semantic facts:

- property definitely numeric
- property definitely text
- expression nullable / non-null
- pattern statically contradictory
- edge label fixed to a single value
- node label set narrowed
- endpoint constraints available
- aggregate column typing

Near-term planner wins:

1. stronger impossible-query fast rejection
2. safer filter pushdown
3. better indexability checks
4. better anchor selection under schema

Status: `done`

Implemented:

- semantic property access export
- semantic aggregate export
- semantic scan/index reasoning annotations
- structured conditional-scan reasoning
- stable explain lines for planner annotations
- narrowing facts used for anchor selection via `narrowing_label_anchor()` (WHERE `IS LABELED` → label cardinality stats)
- WHERE-narrowed labels flow into endpoint contradiction detection (second-pass `check_match_entry_constraints`)
- `build_plan_with_schema_and_stats()`: runs constraint-based type checking during planning, attaches diagnostics to plan
- `PlanAnnotations.type_diagnostics`: type warnings attached to the physical plan
- `PlanAnnotations.statically_contradictory`: flag set when `ImpossiblePattern` is detected
- explain output includes `statically-contradictory=true` and `type-diagnostic-count=N`
- gql_bridge attaches type diagnostics to plan after building
- `SemanticConstraint::WhereEqualityPredicate` / `WhereRangePredicate` / `InlineNodeProperty` extracted during analysis
- `SemanticConstraint::OptionalFilterPredicate` / `InlineNodeWherePredicate` extracted during analysis
- `choose_anchor()` prefers semantic equality predicates, falls back to AST walk for edge cases
- Legacy AST walk functions retained as fallback for complex expressions not yet captured semantically

Remaining:

- (none — schema-endpoint-driven join order is now integrated into `greedy_chain_score`)

## 9. Migration Plan

### Phase A: Introduce semantic analysis object

- add `semantic.rs`
- keep existing `validate.rs`
- make `type_check.rs` consume `SemanticAnalysis` instead of re-deriving everything ad hoc

Deliverable:

- no behavior change
- existing tests still pass

Status: `done`

### Phase B: Convert immediate checks into constraint generation

- preserve current diagnostics
- attach provenance to each diagnostic
- keep `Unknown` semantics unchanged

Deliverable:

- same warning/strict behavior
- richer internals

Status: `done` (infrastructure complete, dual-path parity validated)

Notes:

- `TypedConstraint` enum and `SolvedTypeTable` in semantic.rs
- `ConstraintGenerator` walks AST once, allocating `TypeVarId`s and emitting typed constraints
- `solve_constraints()` produces identical warnings to legacy path
- `type_check_via_constraints()` public entry point with 13 parity tests
- `DiagnosticProvenance` struct for future provenance tracking
- Constraint path is now primary; legacy dead code removed

### Phase C: Integrate full graph type query-time refinement

- expose graph type endpoint/property/type metadata to semantic analysis
- refine node/edge bindings from type annotations and labels
- detect contradictions

Deliverable:

- better diagnostics
- planner facts for impossible patterns

Status: `partial` → `done` (direction-aware, edge-narrowing, multi-hop contradiction detection)

Notes:

- Direction-aware endpoint checking: `Incoming` swaps src/dst, `Either` checks both orientations
- Edge label narrowing from `type(e) = 'X'` in WHERE propagates endpoint constraints via `narrowed_edge_labels`
- Second-pass endpoint check triggers for both `LabelNarrowed` and `EdgeLabelNarrowed` facts
- Multi-hop chains: each hop checked independently against endpoint constraints
- 12 new tests covering incoming/undirected/multi-hop/edge-type-annotation/WHERE-narrowing patterns

### Phase D: Add optional-match null lifting and flow narrowing

- nullable bindings for optional segments
- narrowing after `WHERE`

Deliverable:

- fewer false positives
- better nullability diagnostics

Status: `done`

### Phase E: Add planner exports

- semantic facts passed into planner entry points
- planner uses contradiction and scalar facts

Deliverable:

- measurable planning/runtime wins on typed graphs

Status: `done` (semantic equality/range/inline predicates, type diagnostics, contradiction flag)

## 10. Recommended Non-Goals for the First Refactor

Do not do these in the first iteration:

- full path-shape algebra
- overload resolution based on soft schema heuristics
- principal type synthesis
- complete runtime check elimination
- full IDE/LSP protocol work

The first refactor should focus on making semantic facts explicit and reusable.

## 11. Concrete First Step

The best first implementation step in Gleaph is:

1. add a new semantic pass that produces:
   - typed bindings
   - row schemas for `WITH` / `RETURN` / `NEXT`
   - structured constraints
   - diagnostics
2. rewrite `type_check_statement_with_schema()` to call that pass
3. keep external warning/strict behavior exactly as-is

This gives Gleaph a migration path from:

- direct recursive checking

to:

- semantic analysis feeding both diagnostics and planning

without a risky all-at-once rewrite.

## 12. Updated Completion Snapshot

If the question is "is this design finished?", the precise answer is:

- foundation and first integration wave: `done`
- planner/explain/client surfacing: `done`
- flow-sensitive narrowing (IS NOT NULL, label, edge label): `done`
- OPTIONAL MATCH null lifting: `done`
- `Type::Never` for contradiction detection: `done`
- narrowing fact export to planner: `done`
- aggregation boundary validation (GROUP BY violations): `done`
- WHERE-narrowed label contradiction detection: `done`
- planner narrowing-based anchor selection: `done`
- constraint-based type checking (Phase B infrastructure): `done`
- semantic-first full replacement of legacy typing logic: `done` (constraint path is primary, legacy dead code removed)
- explicit constraint solving instead of direct checks: `done` (TypedConstraint + SolvedTypeTable + solve_constraints)
- planner type diagnostic integration: `done` (build_plan_with_schema_and_stats, statically_contradictory flag)
- semantic-driven anchor selection: `done` (WhereEqualityPredicate, WhereRangePredicate, InlineNodeProperty)
- direction-aware endpoint contradiction: `done` (incoming, undirected, edge-narrowing, multi-hop)
- semantic optional filter capture: `done` (OptionalFilterPredicate for `$param IS NULL OR var.prop <op> $param`)
- semantic inline node WHERE capture: `done` (InlineNodeWherePredicate for `(n WHERE n.prop = val)`)

The design is substantially implemented. The constraint-based pipeline is the
primary type-checking path. Type diagnostics and contradiction status are
exported to the physical plan. Semantic facts drive anchor selection with
legacy AST walks retained as fallback.

- enriched type info structs: `done` (NodeTypeInfo, EdgeTypeInfo, PathTypeInfo with schema metadata)

Remaining future work:
- full `SemanticType` rename (cosmetic, deferred)
- schema-endpoint anchor selection: `done` (schema_endpoint_anchor in planner.rs)
- schema-endpoint-driven multi-hop join order: `done` (greedy_chain_score uses endpoint metadata for unlabeled nodes)
- path-shape semantic typing: `done` (PathTypeInfo with min/max hops from PathLength)
- diagnostic provenance tracking: `done` (WarningProvenance enum on TypeWarning)
