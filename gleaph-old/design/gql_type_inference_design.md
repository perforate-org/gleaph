# Design: Type Inference for Gleaph Graph Query Language

**Status**: Proposed target architecture  
**Audience**: Query engine, planner, semantic analysis, IDE/LSP, and schema subsystem implementers  
**Goal**: Define a practical and extensible type inference architecture for a property-graph query language aligned with GQL-style pattern matching, while remaining implementable in a production engine with partial or evolving schema information.

---

## 1. Summary

This document proposes a **pattern-first, constraint-based, schema-aware, gradually typed** type inference system for Gleaph's Graph Query Language.

The central design choice is:

> Type inference is driven primarily by graph patterns and schema facts, not by Hindley–Milner-style expression inference alone.

The system therefore combines:

- **Pattern-first typing** for `MATCH` and path patterns
- **Constraint-based inference** for expressions, predicates, function calls, and aggregations
- **Schema-aware refinement** using declared schema and/or soft schema
- **Gradual typing** via `Unknown` / nullable / open-row types to support heterogeneous real-world graph data
- **Flow-sensitive narrowing** so `WHERE` predicates refine variable types within the same query segment
- **Boundary typing** at `WITH`, `RETURN`, subqueries, and aggregation boundaries

This architecture is intended to support:

- compile-time validation
- better diagnostics
- IDE completion/hover
- optimizer facts and rewrite safety
- future typed path pattern extensions

---

## 2. Motivation

### 2.1 Why plain Hindley–Milner is not enough

Hindley–Milner is a good fit for local expression typing, but a poor fit for property-graph query languages as a whole.

A graph query language must reason about:

- node, edge, and path variables
- labels and edge types
- property access on heterogeneous entities
- nullability and missing-property behavior
- `OPTIONAL MATCH`
- aggregation and grouping
- subquery boundaries
- schema-backed narrowing
- path pattern validity
- future user-defined procedures/functions

In such languages, a major source of type information is **pattern structure**:

```gql
MATCH (a:Person)-[e:LIKES]->(b:Movie)
WHERE e.weight > 0.8
RETURN a.name, b.title
```

This query determines, before ordinary expression analysis:

- `a` is a vertex
- `b` is a vertex
- `e` is an edge
- `a` carries label `Person`
- `b` carries label `Movie`
- `e` carries edge type `LIKES`
- `e.weight` must be numeric/comparable
- `a.name` and `b.title` must be valid readable properties

This is fundamentally broader than ordinary function-application-based type inference.

### 2.2 Why the type system must be gradual

Real property graphs are often heterogeneous and only partially schema-constrained.

Therefore the type system must:

- preserve useful static guarantees where possible
- tolerate incomplete schema information
- avoid pretending everything is statically known
- support migration toward stricter schema over time

The system should therefore distinguish:

- **known types**
- **nullable known types**
- **unknown but refinable types**
- **fully dynamic / unconstrained values** if needed later

### 2.3 Why the planner should care about types

Type inference is not only an IDE/LSP feature.

A good type inference pipeline produces semantic facts useful to the planner and optimizer:

- whether a property must exist or may be missing
- whether a predicate is numeric/string/temporal
- whether a predicate is contradictory and yields an empty result
- whether a filter can be pushed down safely
- whether a projection is deterministic under grouping
- whether a property access must be guarded at runtime

The type subsystem should therefore be designed as a producer of reusable semantic facts, not only as an error checker.

---

## 3. Design Goals

### 3.1 Primary goals

1. **Sound-enough for production use** under optional schema
2. **Useful diagnostics** with local blame and precise messages
3. **Incremental and implementable** in a Rust codebase
4. **Friendly to IDE/LSP** features
5. **Planner-compatible** semantic fact generation
6. **Extensible** toward richer path typing and user-defined functions/procedures

### 3.2 Non-goals

The initial system does not attempt to provide:

- full dependent typing
- proof-carrying query plans
- complete static elimination of all runtime type errors under schema-optional graphs
- maximal principal types in the Hindley–Milner sense

---

## 4. Architectural Overview

The proposed semantic pipeline is:

1. **Parsing / AST construction**
2. **Name resolution / scope construction**
3. **Pattern typing pass**
4. **Expression constraint generation**
5. **Schema refinement / narrowing**
6. **Constraint solving**
7. **Aggregation and boundary typing**
8. **Typed IR production**
9. **Semantic fact export to planner / IDE**

High-level rule:

- Patterns create variable skeletons.
- Expressions add constraints.
- Schema facts refine those constraints.
- Boundaries finalize visible output types.

---

## 5. Core Type Model

### 5.1 Type categories

The type lattice should minimally support the following categories.

#### Scalar types

- `Bool`
- `Int`
- `Float`
- `Decimal` (optional in early versions)
- `String`
- `Bytes` (optional)
- `Date`
- `Time`
- `Timestamp`
- `Duration`
- `Id` (optional engine-level semantic scalar)

#### Collection / structural types

- `List<T>`
- `Map<K, V>` or `Record<Row>`
- `Tuple<[T...]>` (optional, mainly internal)

#### Graph types

- `Vertex<LabelSet, Row>`
- `Edge<EdgeTypeSet, Row, SrcRef, DstRef>`
- `Path<PathShape>`

#### Meta / gradual types

- `Null<T>` or nullability marker on all types
- `Unknown`
- `Never`
- optionally `Any` later

### 5.2 Nullability model

Nullability should be modeled explicitly rather than encoded ad hoc.

Recommended representation:

- every type has a nullability flag, or
- `Type = BaseType + Nullability`

Examples:

- `Int`
- `Int?`
- `Vertex<Person, {...}>?`

This is necessary for:

- `OPTIONAL MATCH`
- property existence checks
- outer semantics
- nullable builtins
- subquery outputs

### 5.3 Unknown vs Any

The system should distinguish:

#### `Unknown`

Meaning:

- the compiler does not yet know the type
- later predicates/schema may refine it
- solver is allowed to narrow it

#### `Any` (optional later)

Meaning:

- no static restrictions remain
- solver should not expect meaningful narrowing
- mostly a compatibility escape hatch

Early versions can omit `Any` and use only `Unknown` plus explicit runtime-checked casts.

### 5.4 Never

`Never` is useful internally for contradictory constraints, unreachable branches, and static emptiness detection.

Example:

- variable constrained to both label `Person` and edge type `LIKES`
- property required to be both `String` and `Timestamp`

---

## 6. Row Types for Properties

### 6.1 Why row types

Properties on vertices and edges are not naturally modeled as closed structs.

A practical engine needs to support:

- partially known property sets
- label-dependent properties
- future schema refinement
- open-world data ingestion

Therefore vertex/edge properties should be represented as **rows**.

### 6.2 Closed vs open rows

Use two forms:

- **closed row**: exact known property set
- **open row**: known properties plus unknown remainder

Conceptually:

```text
{ name: String, age: Int }
{ name: String, age: Int | ρ }
```

Where `ρ` denotes the remainder of the row.

### 6.3 Recommended representation

A practical internal representation:

```text
Row {
  fields: BTreeMap<PropertyKey, PropertySlot>,
  openness: Open | Closed,
}

PropertySlot {
  ty: Type,
  presence: Required | Optional | UnknownPresence,
}
```

This allows the system to distinguish:

- property definitely exists
- property may exist / may be missing
- property exists but may be null
- property type known vs unknown

### 6.4 Presence vs nullability

Presence and nullability must not be conflated.

A property may be:

- absent
- present with null
- present with non-null value

These are semantically different and matter for optimization and validation.

---

## 7. Graph Entity Types

### 7.1 Vertex type

```text
Vertex<LabelSet, Row>
```

Where:

- `LabelSet` is a possibly imprecise set of labels
- `Row` is the property row for accessible properties

Examples:

- `Vertex<{Person}, {name: String, age: Int? | ρ}>`
- `Vertex<{Person|Employee}, OpenRow>`
- `Vertex<UnknownLabels, OpenRow>`

### 7.2 Edge type

```text
Edge<EdgeTypeSet, Row, SrcRef, DstRef>
```

Where:

- `EdgeTypeSet` captures edge labels/types
- `Row` is edge property row
- `SrcRef` and `DstRef` capture endpoint constraints

`SrcRef` / `DstRef` can initially be coarse.

Initial version may use:

- `AnyVertexRef`

Later versions may refine to:

- source and destination label constraints
- endpoint row constraints

### 7.3 Path type

Initial version:

```text
Path
```

Preferred extensible version:

```text
Path<PathShape>
```

Where `PathShape` may later encode:

- endpoint constraints
- allowed edge kinds
- repetition bounds
- simple/trail/walk semantics

Recommendation:

Start with `Path` as a distinguished type, but design the IR so path shape metadata can be attached later without breaking the public type model.

---

## 8. Constraint Language

The inference engine should operate over explicit constraints.

### 8.1 Minimal constraint set

Recommended core constraints:

- `IsVertex(x)`
- `IsEdge(x)`
- `IsPath(x)`
- `HasLabel(x, L)`
- `HasEdgeType(x, E)`
- `HasProp(x, key, T, Presence)`
- `Comparable(T)`
- `Orderable(T)`
- `Numeric(T)`
- `Temporal(T)`
- `EqCompatible(T1, T2)`
- `Subtype(T1, T2)`
- `JoinableRows(R1, R2)`
- `Nullable(T)` / `NonNull(T)`
- `Aggregatable(fn, T)`
- `Callable(sig, args, ret)`
- `SameEntity(x, y)`
- `PathEndpoint(p, src, dst)`

### 8.2 Derived constraints

The solver may derive additional facts such as:

- `NonNull(x.prop)` after `x.prop IS NOT NULL`
- `HasLabel(x, Person)` after type predicate / pattern annotation
- `Never(x)` after contradiction
- `MayBeMissing(x.prop)` from open row + absent schema fact

### 8.3 Hard vs soft constraints

Useful distinction:

- **hard constraints**: violation is an error
- **soft constraints**: violation becomes warning or runtime check

Examples:

- invalid function arity: hard
- accessing unknown property under permissive schema mode: soft or deferred
- contradictory label/path shape: hard if statically provable

---

## 9. Inference Pipeline

## 9.1 Pass 0: Scope and symbol setup

Before typing:

- build lexical scopes
- register variable declarations/bindings
- resolve references to prior `WITH` / `RETURN` aliases
- identify aggregation boundaries
- classify built-in and user-defined function references

Output:

- scope graph
- symbol table skeleton

## 9.2 Pass 1: Pattern-first typing

This pass assigns graph-entity skeletons to variables introduced by graph patterns.

### Responsibilities

- recognize vertex, edge, path bindings
- assign initial graph kinds
- accumulate label / edge-type facts
- enforce variable reuse consistency
- capture directionality constraints
- mark optional-origin variables for nullability lifting

### Example

```gql
MATCH (a:Person)-[e:LIKES]->(b:Movie)
```

Produces initial environment:

- `a : Vertex<{Person}, OpenRow>`
- `e : Edge<{LIKES}, OpenRow, AnyVertexRef, AnyVertexRef>`
- `b : Vertex<{Movie}, OpenRow>`

### Reuse example

```gql
MATCH (a)-[e]->(b), (b)-[f]->(c)
```

Adds:

- `SameEntity(b_from_first_pattern, b_from_second_pattern)`

which resolves to one binding.

## 9.3 Pass 2: Expression constraint generation

Walk expressions and emit constraints.

### Examples

#### Property access

```gql
n.age
```

Emits:

- `HasProp(n, "age", T_age, Presence_age)`

#### Numeric comparison

```gql
n.age > 18
```

Emits:

- `HasProp(n, "age", T_age, Presence_age)`
- `Numeric(T_age)`
- `Orderable(T_age)`
- `NonNull(n.age)` if comparison semantics require null rejection in the static environment for the true branch

#### Function call

```gql
sum(e.weight)
```

Emits:

- `HasProp(e, "weight", T_w, Presence_w)`
- `Aggregatable(sum, T_w)`
- result type variable `T_ret`

### Expression typing style

Use **bidirectional typing** locally:

- synthesize from literals and local expressions where possible
- use expected type from context where available

Examples of contexts with expectations:

- `WHERE` expects `Bool`
- function arguments may be overload-directed
- `CASE` branches should join to a common supertype
- projection aliases may carry declared/expected types later

## 9.4 Pass 3: Flow-sensitive narrowing

Predicates should refine variable types inside the same clause pipeline.

### Supported refinements

- type predicate or label predicate
- `IS NOT NULL`
- existence checks
- equality with typed literal
- successful cast checks (future)

### Example

```gql
MATCH (n)
WHERE n:Person AND n.age IS NOT NULL AND n.age > 18
RETURN n.age
```

Refinement sequence:

1. `n : Vertex<UnknownLabels, OpenRow>`
2. after `n:Person` -> `n : Vertex<{Person}, OpenRow>`
3. after `n.age IS NOT NULL` -> property presence strengthened and nullability removed in true branch
4. after `n.age > 18` -> `age` narrowed to numeric/orderable

### Boolean structure

Refinement should be branch-sensitive.

For `AND`:

- right side sees refinements from left side if left is true

For `OR`:

- branch environments must be joined conservatively

For `NOT`:

- refinements may invert only for explicitly supported predicates; otherwise keep conservative

## 9.5 Pass 4: Schema refinement

Apply declared schema and/or soft schema observations.

### Declared schema mode

If the graph has declared schema, use it to:

- validate labels and edge types
- refine property presence and property types
- reject impossible patterns earlier
- improve diagnostics

### Soft schema mode

If the graph is schema-optional, use a soft schema source to infer likely facts:

- observed property types by label/edge type
- property existence frequencies
- conflicting observed types
- endpoint type distributions for edge kinds

Soft schema facts are not absolute truth.

They should produce:

- narrowing when safe
- warnings when suspicious
- runtime checks where necessary

### Conflict handling

If schema says `Person.age : Int` but an expression requires `Timestamp`:

- in strict mode: error
- in permissive mode: warning or typed runtime cast/check if semantics allow

## 9.6 Pass 5: Constraint solving

The solver resolves type variables, row obligations, nullability, and contradictions.

Recommended characteristics:

- union-find or equivalent for ordinary type equality classes
- explicit solvers for row/property obligations
- explicit lattice joins for branch merging
- branch-aware environment join
- contradiction tracking to produce `Never`

The solver should preserve provenance for diagnostics.

### Provenance requirement

Each constraint should store:

- AST span
- source clause
- originating rule
- whether it is hard/soft

This is essential for good error messages.

## 9.7 Pass 6: Aggregation and boundary typing

Finalize types at:

- `WITH`
- `RETURN`
- subquery outputs
- set operations if added later

This pass checks:

- grouping legality
- aggregate argument validity
- projection result types
- alias visibility
- nullability changes induced by optionality or aggregation semantics

---

## 10. Typing Rules by Clause

## 10.1 `MATCH`

### Responsibilities

- introduce graph bindings
- assign vertex/edge/path kinds
- attach label/type constraints
- unify reused variables

### Result

`MATCH` primarily extends the environment.

It does not itself require a boolean result, unlike `WHERE`.

## 10.2 `OPTIONAL MATCH`

`OPTIONAL MATCH` should lift newly bound variables into nullable form.

Example:

```gql
MATCH (a:Person)
OPTIONAL MATCH (a)-[e:LIKES]->(b:Movie)
RETURN e, b
```

After the optional clause:

- `e : Edge<{LIKES}, ...>?`
- `b : Vertex<{Movie}, ...>?`

### Important rule

Variables bound before the `OPTIONAL MATCH` remain non-null unless separately nullable for other reasons.

## 10.3 `WHERE`

`WHERE` expects a boolean result.

Typing obligations:

- expression must be `Bool`-compatible
- predicates may refine environment
- impossible conjunctions may yield static emptiness warning/error

## 10.4 `WITH`

`WITH` is a major scope and typing boundary.

Responsibilities:

- compute projected output row
- apply aggregation legality checks
- discard non-projected variables from downstream scope
- freeze inferred types for exported aliases

Recommendation:

Represent each `WITH` as producing a typed binding row analogous to a relational projection schema.

## 10.5 `RETURN`

`RETURN` behaves like terminal `WITH`.

Responsibilities:

- finalize output row type
- validate grouping
- validate ordering expressions if `ORDER BY` present

## 10.6 Subqueries

Subqueries should be typed with explicit input and output environments.

Recommended rule:

- outer scope variables imported into subquery are treated as read-only inputs
- subquery exports only declared/projected outputs
- nullability and aggregation are resolved inside the subquery before export

---

## 11. Aggregation Model

Aggregation is one of the most important non-local typing concerns.

### 11.1 Aggregate boundary

Within an aggregation scope:

- projected expressions must be either aggregate expressions or grouping keys
- non-aggregated non-grouped expressions are invalid

### 11.2 Aggregate signatures

Example built-ins:

- `count(*) -> Int`
- `count(x) -> Int`
- `sum(Int) -> Int`
- `sum(Float) -> Float`
- `avg(Int|Float|Decimal) -> Float|Decimal` depending on semantics
- `collect(T) -> List<T>`
- `min(T) -> T` where `Orderable(T)`
- `max(T) -> T` where `Orderable(T)`

### 11.3 Null handling

Aggregate typing should model null semantics explicitly.

Examples:

- `count(x)` may ignore nulls but still returns non-null `Int`
- `sum(x)` may return nullable depending on empty-group semantics
- `collect(x)` may or may not include nulls depending on language semantics

The design must record the engine's exact semantics and not leave them implicit.

---

## 12. Function and Procedure Typing

## 12.1 Built-in functions

Built-ins should use declarative signatures with constraint predicates.

Example:

```text
length : Path -> Int
labels : Vertex<L, R> -> List<String>
startNode : Edge<E, R, S, D> -> Vertex<..., ...>
```

Where exact endpoint reconstruction may initially be coarse.

## 12.2 Overloading

If overloading exists, the resolver should:

1. infer argument constraints
2. enumerate viable overloads
3. prefer most specific applicable overload
4. produce ambiguity error if multiple remain

### Recommendation

Keep early versions conservative. Avoid excessive overloading until diagnostics are mature.

## 12.3 User-defined functions

Planned architecture should support:

- declared signatures
- purity metadata if useful for planner
- nullability contracts
- determinism metadata

User-defined procedures may require a separate effect model later.

---

## 13. Soft Schema

## 13.1 Purpose

Soft schema allows the engine to remain useful even without strict DDL.

It may be derived from:

- observed data statistics
- sampled scans
- write-time schema accumulation
- schema declarations when available

## 13.2 Suggested stored facts

Per label / edge type:

- observed property names
- observed types per property
- observed nullability / presence ratios
- endpoint label distributions for edge kinds
- conflict counts

## 13.3 Using soft schema in typing

Soft schema should be used to:

- refine `Unknown`
- prioritize likely overload resolution
- improve diagnostics and completion
- warn about suspicious or contradictory usage

### Example diagnostic

> Property `timestamp` on edge type `LIKES` is not declared and has not been observed in the soft schema.

### Important rule

Soft schema must not silently become hard truth in permissive mode.

---

## 14. Emptiness and Contradiction Detection

The type system should detect semantically impossible queries when feasible.

Examples:

- variable simultaneously forced to be vertex and edge
- label/type combination impossible under schema
- endpoint constraints impossible for an edge type
- property used as both numeric and temporal in same solved environment

Output options:

- hard error for definitive contradiction
- warning for likely contradiction under soft schema
- planner fact `ResultIsStaticallyEmpty`

This is valuable for both user feedback and optimizer behavior.

---

## 15. Typed IR

The result of semantic analysis should be a typed IR rather than only decorated AST nodes.

### 15.1 Typed expression node should carry

- solved type
- nullability
- optionality/missing-property behavior if relevant
- source span
- simplification or cast nodes inserted by typing

### 15.2 Typed pattern node should carry

- bound variables
- inferred/specified labels and edge types
- endpoint relationships
- optionality origin
- static contradiction flags if any

### 15.3 Exported semantic facts

The typed IR should expose facts to the planner such as:

- property definitely non-null
- comparison is numeric
- expression deterministic
- branch contradiction detected
- projected row schema
- grouping keys
- eligible pushdowns

---

## 16. Diagnostics

The quality of error messages matters as much as soundness.

### 16.1 Diagnostic principles

- point to the smallest blameable span
- report expected vs actual type/shape
- mention relevant prior constraint source if conflict is relational
- distinguish error vs warning vs runtime-check insertion
- explain optionality/missing-property issues clearly

### 16.2 Example messages

#### Invalid numeric use

> Property `age` on `n` is used as a numeric value here, but the inferred type is `String`.

#### Missing property under strict mode

> Property `weight` is not available on edge type `LIKES` under the active schema.

#### Grouping violation

> `a.name` appears in the projection but is neither grouped nor aggregated.

#### Contradiction

> This pattern is statically unsatisfiable: edge type `LIKES` cannot connect `Movie` to `Movie` under the active schema.

---

## 17. IDE / LSP Integration

The type system should be designed to support incremental IDE use.

### 17.1 Required capabilities

- incomplete query tolerance
- partial constraint solving
- best-effort types at cursor position
- completion candidates based on narrowed variable type
- hover showing inferred type plus nullability and source of certainty

### 17.2 Confidence levels

Useful hover metadata:

- declared by schema
- inferred from pattern
- refined by predicate
- guessed from soft schema
- unresolved / unknown

This makes the developer experience much more trustworthy.

---

## 18. Planner Integration

## 18.1 Why integrate

Typing and planning should remain separate modules, but they should share semantic facts.

### Useful facts for planner

- variable graph kind
- label/type narrowing
- property existence / non-null guarantees
- contradiction / static emptiness
- aggregate boundary facts
- deterministic vs nondeterministic expressions

## 18.2 Suggested interface

Expose a semantic summary object per query block:

```text
SemanticFacts {
  variable_types,
  property_facts,
  contradiction_flags,
  grouping_info,
  output_row,
  pushdown_safe_predicates,
}
```

The planner consumes this object but does not mutate typing results.

---

## 19. Modes of Strictness

Different deployments may want different static behavior.

Recommended modes:

### 19.1 Strict mode

- declared schema required or strongly enforced
- missing properties are errors where statically visible
- contradictory patterns become errors
- soft schema not used to excuse violations

### 19.2 Permissive mode

- unknown properties allowed with `Unknown` / runtime checks
- soft schema informs warnings and completions
- only definitive contradictions are hard errors

### 19.3 Recommended default

For early product versions:

- permissive execution semantics
- strong warnings
- optional strict mode for CI / production query validation

---

## 20. Implementation Strategy in Rust

## 20.1 Core data structures

Suggested direction:

- `TypeId` interned arena handle
- canonicalized `Type` storage
- `RowId` for property rows
- `ConstraintId` for provenance tracking
- persistent/incremental environment maps if IDE support matters early

### Sketch

```rust
pub enum TypeKind {
    Never,
    Unknown,
    Bool,
    Int,
    Float,
    Decimal,
    String,
    Date,
    Time,
    Timestamp,
    Duration,
    List(TypeId),
    Record(RowId),
    Vertex(VertexType),
    Edge(EdgeType),
    Path(PathType),
}

pub struct Type {
    pub kind: TypeKind,
    pub nullability: Nullability,
}

pub enum Nullability {
    NonNull,
    Nullable,
}
```

### Row sketch

```rust
pub struct RowType {
    pub fields: BTreeMap<Symbol, PropertySlot>,
    pub openness: RowOpenness,
}

pub struct PropertySlot {
    pub ty: TypeId,
    pub presence: Presence,
}

pub enum Presence {
    Required,
    Optional,
    Unknown,
}
```

## 20.2 Constraint storage

Each constraint should be a first-class object with provenance.

```rust
pub struct Constraint {
    pub kind: ConstraintKind,
    pub span: Span,
    pub source_clause: ClauseId,
    pub severity: ConstraintSeverity,
}
```

## 20.3 Solver organization

Do not attempt a single monolithic solver initially.

Prefer layered solving:

1. equality/unification-like solving for ordinary type variables
2. row/property obligation solving
3. nullability refinement
4. branch join / lattice merge
5. overload resolution
6. contradiction detection

This is easier to debug and much easier to explain in diagnostics.

---

## 21. Minimal Viable Version

Recommended MVP scope:

### Supported

- scalar typing
- `Vertex` / `Edge` / `Path`
- open rows
- property access constraints
- label/type narrowing from `MATCH`
- `WHERE` boolean checking
- `IS NOT NULL` narrowing
- aggregate boundary checking
- `WITH` / `RETURN` typed outputs
- soft schema hook

### Deferred

- precise typed path shapes
- full endpoint typing on edges
- advanced casts
- user-defined function overloading
- effect typing for procedures
- full branch theorem proving

This MVP is already enough to materially improve correctness, error quality, and optimization.

---

## 22. Roadmap to Final Form

## Phase 1: Foundational typing

- implement type arena and typed AST/IR
- implement pattern-first variable skeletons
- implement scalar and property constraints
- implement nullability and `OPTIONAL MATCH`
- implement boundary typing for `WITH` / `RETURN`

## Phase 2: Soft schema and diagnostics

- integrate schema facts
- improve property existence typing
- add high-quality diagnostics with provenance
- expose LSP hover/completion summaries

## Phase 3: Planner integration

- export semantic facts to optimizer
- mark pushdown-safe predicates
- detect static emptiness
- use typing facts in rewrite validation

## Phase 4: Advanced path typing

- enrich `Path` with shape metadata
- endpoint-aware path typing
- bounded repetition constraints
- contradiction detection for typed paths

## Phase 5: Strict validation mode

- CI-friendly static query validation
- stronger schema conformance
- compatibility reporting for evolving schema

---

## 23. Open Design Questions

1. Should missing property access produce `Null`, `Unknown`, or a distinct `Missing` semantic value in the type system?
2. How much endpoint information should `Edge` carry in the first public type model?
3. Should `Id` be a first-class semantic scalar or remain engine-internal?
4. How aggressively should soft schema influence overload resolution?
5. How should vector / embedding properties fit into the scalar lattice if supported later?
6. Should type predicates become first-class syntax in Gleaph, and if so, should they refine only within clause-local flow or across `WITH` boundaries when re-projected?

These should be settled before locking down the public semantic model.

---

## 24. Final Recommendation

Gleaph should adopt a **pattern-first, constraint-based, schema-aware, gradually typed** inference architecture.

Concretely:

- Do **not** center the design on Hindley–Milner.
- Use HM-style local synthesis only for small expression fragments.
- Treat patterns and schema as primary sources of type information.
- Represent vertex/edge properties with open rows.
- Distinguish nullability from property presence.
- Keep `Unknown` as a first-class type to support schema-optional graphs.
- Refine types flow-sensitively through `WHERE`.
- Make typed semantic facts available to both IDE and planner.
- Reserve room for richer typed path patterns later.

This approach is the best balance of rigor, implementability, and long-term extensibility for a production graph query engine.

---

## 25. Appendix A: Worked Example

```gql
MATCH (a:Person)-[e:LIKES]->(b:Movie)
WHERE e.weight IS NOT NULL AND e.weight > 0.8
WITH a, b, e.weight AS w
RETURN a.name, b.title, w
```

### Step 1: Pattern typing

- `a : Vertex<{Person}, OpenRow>`
- `e : Edge<{LIKES}, OpenRow, AnyVertexRef, AnyVertexRef>`
- `b : Vertex<{Movie}, OpenRow>`

### Step 2: Expression constraints

From `e.weight`:

- `HasProp(e, "weight", T_w, P_w)`

From `e.weight IS NOT NULL`:

- in true branch, `NonNull(T_w)` and strengthen presence if semantics allow

From `e.weight > 0.8`:

- `Numeric(T_w)`
- `Orderable(T_w)`

### Step 3: Schema refinement

Suppose soft/declared schema says:

- `LIKES.weight : Float`

Then solve:

- `T_w = Float`

### Step 4: `WITH`

Exported environment:

- `a : Vertex<{Person}, ...>`
- `b : Vertex<{Movie}, ...>`
- `w : Float`

### Step 5: `RETURN`

Property obligations:

- `HasProp(a, "name", String, Required|Optional)`
- `HasProp(b, "title", String, Required|Optional)`

Final output row:

- `name : String`
- `title : String`
- `w : Float`

---

## 26. Appendix B: Worked Optional Example

```gql
MATCH (a:Person)
OPTIONAL MATCH (a)-[e:LIKES]->(b:Movie)
RETURN a.name, b.title
```

After `OPTIONAL MATCH`:

- `a : Vertex<{Person}, ...>`
- `e : Edge<{LIKES}, ...>?`
- `b : Vertex<{Movie}, ...>?`

Then `b.title` becomes nullable unless language semantics mandate runtime null propagation differently.

Result:

- `a.name : String`
- `b.title : String?`

---

## 27. Appendix C: Suggested Internal Terminology

To keep implementation discussions clear, use the following terminology consistently:

- **type inference**: overall process
- **pattern typing**: graph pattern-based variable typing
- **constraint generation**: rule-based extraction of obligations
- **narrowing**: flow-sensitive refinement
- **schema refinement**: use of declared/soft schema to sharpen types
- **boundary typing**: finalization at clause boundaries
- **semantic facts**: planner/IDE-consumable solved facts
- **presence**: whether a property exists
- **nullability**: whether a present value may be null

This vocabulary will reduce confusion across parser, analyzer, planner, and IDE discussions.

