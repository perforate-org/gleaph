# Static Type System (§18.9)

## Status: Phase 1+2 complete (48 tests). Phase 3 (error-mode) optional.

> Pattern type annotations `(n :: PersonType)` in MATCH, DEFINE node types in GraphType,
> runtime label filtering, temporal types, and BYTES type all complete.
> Schema-aware inference via `PropertySchema` trait, WITH/NEXT type propagation done.
> **Remaining**: Error-mode (Phase 3), type union syntax, NOT NULL constraint propagation.

## Motivation

GQL specifies type inference and static type checking. Currently Gleaph is fully
dynamically typed — all type errors are discovered at runtime. A static type system
catches errors earlier and enables query optimization.

## Current State

- `ValueType` enum: Int, Float, Text, Bool, Timestamp, List, Null, Bytes
- `CAST(expr AS type)` and `IS :: type` are runtime-only
- Parameter type annotations (`$x :: INT`) are informational warnings
- No type inference on expressions

## Design

### Type Representation

```rust
/// Static type of an expression, inferred during planning.
#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    /// Concrete scalar types.
    Scalar(ValueType),
    /// Union of possible types (e.g., result of COALESCE).
    Union(Vec<Type>),
    /// List with known element type.
    TypedList(Box<Type>),
    /// Unknown type (not yet inferred).
    Unknown,
    /// Node reference with known labels.
    Node(Vec<String>),
    /// Edge reference with known label.
    Edge(Option<String>),
    /// Path type.
    Path,
    /// Record type with named fields.
    Record(Vec<(String, Type)>),
}
```

### TypeEnv

```rust
/// Type environment mapping variable names to their inferred types.
pub struct TypeEnv {
    bindings: HashMap<String, Type>,
}

impl TypeEnv {
    fn infer_expr(&self, expr: &Expr) -> Type { ... }
    fn check_expr(&self, expr: &Expr, expected: &Type) -> Result<(), TypeError> { ... }
    fn bind(&mut self, name: &str, ty: Type) { ... }
}
```

### Inference Rules

| Expression | Inferred Type |
|---|---|
| `42` | `Scalar(Int)` |
| `3.14` | `Scalar(Float)` |
| `'hello'` | `Scalar(Text)` |
| `true` | `Scalar(Bool)` |
| `X'AB'` | `Scalar(Bytes)` |
| `null` | `Scalar(Null)` |
| `[a, b, c]` | `TypedList(Union of element types)` |
| `a + b` (Int, Int) | `Scalar(Int)` |
| `a + b` (Float, _) | `Scalar(Float)` |
| `a + b` (Text, Text) | `Scalar(Text)` |
| `a = b` | `Scalar(Bool)` |
| `COALESCE(a, b)` | `Union(type(a), type(b))` minus Null |
| `CASE WHEN ... THEN a ELSE b` | `Union(type(a), type(b))` |
| `n.prop` (Node) | `Unknown` (no property type info) |
| `CAST(e AS T)` | `Scalar(T)` |
| `$x :: INT` | `Scalar(Int)` |

### Type Checking Rules

| Context | Check |
|---|---|
| `a + b` | Both numeric or both text/list |
| `a AND b` | Both boolean |
| `WHERE expr` | `expr` must be boolean |
| `RETURN n.prop` | `n` must be node/edge binding |
| `id(n)` | `n` must be node binding |
| `source(e)` | `e` must be edge binding |

### Implementation Phases

#### Phase 1: Warning Mode
- Add `TypeEnv` to planner
- Infer types for all expressions
- Emit warnings (not errors) for type mismatches
- Store inferred types in `PhysicalPlan` for debugging

#### Phase 2: Strict Mode for Known Types
- Errors for provably wrong operations:
  - `42 + 'hello'` (Int + Text without concat context)
  - `WHERE 42` (non-boolean WHERE)
  - `id('hello')` (wrong argument type)
- Continue allowing unknown types (property accesses)

#### Phase 3: Full Strictness (Optional)
- Require type annotations on parameters
- Integrate with GraphType schema for property types
- Reject ambiguous operations

### Integration Points

- **Planner**: Type-aware cost estimation (e.g., index lookups for typed equality)
- **Executor**: Skip runtime type checks when types are statically known
- **GraphType (W-D5)**: Property type constraints feed back into TypeEnv
- **Error messages**: Include expected vs actual type information

### Risks

- **Backward compatibility**: Existing queries that work dynamically might fail static checks
  → Mitigated by phased rollout (warnings first)
- **Performance overhead**: Type inference adds planning time
  → Expected to be negligible (single pass over AST)
- **Complexity**: Union types and unknown types make inference non-trivial

### Test Plan

- Unit tests for type inference on each expression kind
- Regression: all existing tests must continue to pass in warning mode
- New tests for type error detection
- Integration tests with GraphType property constraints
