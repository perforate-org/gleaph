# GQL Parameter Type Inference Plan

## Goal

Infer parameter types for unannotated prepared-statement parameters from typed usage sites, so queries like:

```gql
MATCH (u:User)
WHERE u.age >= $min_age
  AND u.name = $name
RETURN u.name AS name
```

produce prepared metadata roughly equivalent to:

- `$min_age`: `INT`
- `$name`: `TEXT`

without requiring explicit annotations on every parameter.

The immediate product goal is stronger prepared metadata for codegen:

- Rust: prefer native Rust types over `Value` where safe
- JS/TS: emit richer docs and, later, stronger generated types
- Runtime: keep current semantics for unresolved/ambiguous parameters

## Current State

### What exists

- Parser-level parameter annotations already exist:
  - `$x :: INT`
  - `$x :: INT | NULL`
  - implemented in [parser.rs](/Users/yota/dev/gleaph/crates/gql/src/parser.rs)
- Static expression inference exists:
  - implemented in [type_check.rs](/Users/yota/dev/gleaph/crates/gql/src/type_check.rs)
  - `Expr::Parameter` with no annotation currently becomes `Type::Unknown`
- Prepared metadata exists, but only carries:
  - `name`
  - `required`
  - defined in [lib.rs](/Users/yota/dev/gleaph/crates/types/src/lib.rs)
- Prepared parameter collection currently only determines optionality from `| NULL`:
  - implemented in [executor.rs](/Users/yota/dev/gleaph/crates/gql/src/executor.rs)

### Gap

No reverse propagation currently exists from typed contexts into unannotated parameters. In particular:

- `u.age >= $min_age` does not currently infer `$min_age: INT`
- `u.name = $name` does not currently infer `$name: TEXT`

## External References

These references support the overall direction, even if they do not prescribe the exact implementation shape for Gleaph:

- Gleaph already models parameter type annotations and `NULL` unions in its AST/parser, which matches the typed-parameter direction in GQL-like languages.
- openCypher materials emphasize semantic analysis and type reasoning over property-graph queries:
  - [openCypher resources](https://opencypher.org/resources/)
- Property graph query language research generally treats type inference/checking as semantic-analysis work layered over the parsed AST and schema knowledge. A representative search direction is ACM/arXiv literature on static typing for graph query languages.

Pragmatic note: for this feature, the repo’s existing type system and schema-aware inference machinery are a stronger implementation guide than any single paper.

## Non-Goals

- Full Hindley-Milner-style inference for all expressions
- Changing runtime query semantics for unresolved parameters
- Emitting row/return types for all codegen outputs in the same change
- Inferring exact list element types in every case

## Design Principles

1. Explicit annotation wins.
2. Inference should only strengthen metadata when confidence is high.
3. Ambiguity falls back to `Unknown`, not a guess.
4. Runtime behavior remains backward-compatible unless a parameter is explicitly annotated and invalid.
5. Prepared metadata should preserve enough structure for future codegen improvements.

## Proposed Data Model Changes

### 1. Add prepared parameter type metadata

Extend [lib.rs](/Users/yota/dev/gleaph/crates/types/src/lib.rs):

```rust
pub enum PreparedValueType {
    Int,
    Float,
    Text,
    Bool,
    Timestamp,
    List,
    Null,
    Bytes,
    Date,
    Time,
    DateTime,
    Duration,
    Principal,
}

pub struct PreparedParameterInfo {
    pub name: String,
    pub required: bool,
    pub types: Vec<PreparedValueType>,
    pub inferred: bool,
}
```

Semantics:

- `types.len() == 0`: unresolved / unknown
- one entry: concrete scalar/list type
- multiple entries: union
- `inferred = false`: came from explicit annotation
- `inferred = true`: came from reverse inference and may be conservative

### 2. Carry type diagnostics in prepared metadata

Optional but recommended:

```rust
pub struct TypeDiagnostic {
    pub parameter: String,
    pub detail: String,
}
```

This is useful when inference finds conflicting evidence but we choose not to fail `prepare`.

## Inference Strategy

Implement parameter inference as a separate semantic-analysis pass, not by mutating the existing parser AST.

### Pass shape

Add a new module in `crates/gql/src`, for example:

- `param_inference.rs`

Entry point:

```rust
pub fn infer_parameter_types_with_schema(
    stmt: &Statement,
    schema: &dyn PropertySchema,
) -> BTreeMap<String, InferredParameterType>
```

Where:

```rust
pub struct InferredParameterType {
    pub types: Vec<Type>,
    pub explicit: bool,
    pub required: bool,
    pub conflicts: Vec<String>,
}
```

### Rules

#### Rule A: Explicit annotation wins

If `$x :: INT | NULL` exists anywhere, record:

- types = `INT | NULL`
- explicit = true

Any inferred evidence must be compatible with that. Incompatible usage becomes a warning or strict-mode error.

#### Rule B: Reverse-infer from comparisons

If one side is a parameter and the other side has a known scalar type:

- `$x = u.age` and `u.age: INT` -> infer `$x: INT`
- `u.name = $name` and `u.name: TEXT` -> infer `$name: TEXT`
- apply to `=`, `<>`, `<`, `<=`, `>`, `>=`

Do not infer if the other side is `Unknown`.

#### Rule C: Reverse-infer from arithmetic

If a parameter participates in numeric arithmetic with a known numeric type:

- `$x + 1` -> `INT`
- `$x + u.price` where `u.price: FLOAT` -> `FLOAT`

Conservative behavior:

- mixed `INT`/`FLOAT` evidence widens to `FLOAT`
- unsupported numeric-temporal combos remain `Unknown`

#### Rule D: Reverse-infer from string predicates

- `u.name STARTS WITH $prefix` -> `$prefix: TEXT`
- `u.bio CONTAINS $needle` -> `$needle: TEXT`

#### Rule E: Reverse-infer from built-in functions

Add a small table of parameter positions with expected types, for example:

- `id(x)` -> `x` must be node/edge, not relevant for parameter inference
- `to_string($x)` does not constrain `$x`
- `date($x)` or temporal constructors may constrain to `TEXT` if parser/runtime semantics require text input

Start small. Only add rules where the runtime contract is already clear.

#### Rule F: Nullability

Optionality continues to be driven by explicit `| NULL`, not by inferred usage.

Reason:

- nullability inference is much more fragile
- existing prepared runtime semantics already key off explicit `| NULL`

## Conflict Resolution

Given multiple evidence sources:

- same type repeated -> keep one
- `INT` + `FLOAT` -> widen to `FLOAT`
- `TEXT` + `INT` -> conflict, keep `Unknown` unless one side was explicit
- explicit + incompatible inferred -> diagnostic, and strict mode may reject

Recommended initial behavior:

- `prepare()` succeeds
- parameter gets `types = []` when evidence conflicts
- conflict goes into diagnostics / warning path

## Integration Plan

### Phase 1: Metadata only

1. Add `PreparedValueType` and extend `PreparedParameterInfo`
2. Add parameter inference pass in `crates/gql`
3. In [gql_bridge.rs](/Users/yota/dev/gleaph/crates/graph/src/gql_bridge.rs), replace current parameter collection with:
   - explicit annotation extraction
   - reverse inference merge
   - optionality extraction
4. Persist the richer metadata in [state.rs](/Users/yota/dev/gleaph/crates/graph/src/state.rs)
5. Update:
   - [graph.did](/Users/yota/dev/gleaph/crates/graph/graph.did)
   - [sdk/js/src/types.ts](/Users/yota/dev/gleaph/sdk/js/src/types.ts)
   - [sdk/js/src/idl.ts](/Users/yota/dev/gleaph/sdk/js/src/idl.ts)

Success criteria:

- `list_prepared()` shows parameter type metadata
- no codegen changes required yet

### Phase 2: Codegen consumption

#### Rust

Use stronger native types when:

- explicit or inferred single type exists
- nullable union with one non-null type -> `Option<T>`
- unresolved or complex union -> `Value`

Examples:

- `INT` -> `i64`
- `TEXT` -> `String`
- `BOOL` -> `bool`
- `PRINCIPAL` -> `Principal`
- `INT | NULL` -> `Option<i64>`
- `INT | TEXT` -> generated enum
- unknown -> `Value`

#### JS/TS

Initial low-risk use:

- improve generated docs
- keep runtime behavior unchanged

Possible later step:

- emit TS field types from inferred metadata where safe

### Phase 3: Diagnostics and strictness

Wire inference conflicts into existing type-check behavior:

- warning mode: include in prepared diagnostics
- strict mode: reject incompatible explicit/inferred combinations

## Code Locations to Touch

- [type_check.rs](/Users/yota/dev/gleaph/crates/gql/src/type_check.rs)
  - reuse `Type`, `PropertySchema`, and existing expression inference where possible
- [executor.rs](/Users/yota/dev/gleaph/crates/gql/src/executor.rs)
  - replace current prepared parameter collector or move it into semantic layer
- [gql_bridge.rs](/Users/yota/dev/gleaph/crates/graph/src/gql_bridge.rs)
  - surface metadata in `prepare_statement` and `list_prepared`
- [state.rs](/Users/yota/dev/gleaph/crates/graph/src/state.rs)
  - persist richer metadata
- [lib.rs](/Users/yota/dev/gleaph/crates/types/src/lib.rs)
  - public API types
- [mod.rs](/Users/yota/dev/gleaph/crates/cli/src/codegen/mod.rs)
  - shared codegen type mapping
- [rust_lang.rs](/Users/yota/dev/gleaph/crates/cli/src/codegen/rust_lang.rs)
  - native Rust type emission

## Test Plan

### Parser / semantic tests

- explicit annotation preserved:
  - `$x :: INT`
- optional annotation preserved:
  - `$x :: INT | NULL`
- reverse inference from comparison:
  - `u.age > $min_age` -> `INT`
  - `u.name = $name` -> `TEXT`
- reverse inference from string predicates:
  - `u.name STARTS WITH $prefix` -> `TEXT`
- numeric widening:
  - `u.score + $delta` where `score: FLOAT` -> `FLOAT`
- conflict:
  - one usage implies `INT`, another implies `TEXT` -> unresolved

### Prepared metadata tests

- `prepare_statement()` returns typed parameters
- `list_prepared()` preserves typed parameters after storage
- restore from snapshot preserves typed parameters

### Codegen tests

- Rust:
  - `INT` -> `i64`
  - `TEXT | NULL` -> `Option<String>`
  - unresolved -> `Value`
  - union -> generated enum
- JS/TS:
  - docs show inferred types
  - optional fields stay optional

## Rollout Recommendation

1. Ship metadata first.
2. Gate Rust strong typing behind the presence of explicit/inferred metadata.
3. Keep unknown parameters as `Value` until inference coverage is proven.
4. Add conflict diagnostics before enabling any stricter behavior.

## Recommended First Slice

The smallest valuable implementation is:

1. Add `types` + `inferred` to `PreparedParameterInfo`
2. Infer from:
   - property-vs-parameter comparisons
   - string predicates
3. Keep nullability explicit-only via `| NULL`
4. Expose metadata in `prepare` and `list_prepared`
5. Update Rust codegen only for:
   - single inferred scalar type
   - `single_type | NULL`

That gets the main user-facing win without committing to full general-purpose inference.
