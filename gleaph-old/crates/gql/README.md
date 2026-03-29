# gleaph-gql

GQL query engine for the Gleaph graph database.

This crate implements a complete pipeline for parsing, validating, planning,
and executing a subset of the [GQL graph query language](https://www.iso.org/standard/76120.html)
against an in-memory `gleaph-pma` graph.

## Crate layout

| Module     | Role                                                     |
| ---------- | -------------------------------------------------------- |
| `lexer`    | Tokenise a raw query string into `Token`s                |
| `parser`   | Recursive-descent parser → `ast::Statement`              |
| `ast`      | AST types (`Statement`, `QueryStmt`, `NodePattern`, …)   |
| `validate` | Semantic validation (scope, feature gates, safety rules) |
| `planner`  | Heuristic anchor selection → `PhysicalPlan`              |
| `plan`     | `PhysicalPlan` / `PlanOp` types                          |
| `executor` | Interprets a plan against a `PmaGraph`                   |
| `value`    | Cross-type `Value` comparison utilities                  |

## Pipeline

```
query string
  └─ lexer::tokenize
       └─ parser::parse_statement  →  ast::Statement
            └─ validate::validate_statement
                 ├─ (query)    planner::build_plan  →  PhysicalPlan
                 │                └─ executor::execute_plan  →  QueryResult
                 └─ (mutation) executor::execute_mutation  →  MutationResult
```

## Supported syntax

### Query

```gql
MATCH (a:User)-[e:FOLLOWS]->(b:User)
WHERE a.id = 42 AND b.name <> 'bot'
RETURN a.id, b.name AS name
ORDER BY b.name DESC
LIMIT 10
```

### Create node

```gql
INSERT (:User {id: 1, name: 'Alice'})
```

### Create edge

```gql
INSERT (:User {name: 'Alice'})-[:KNOWS]->(:User {name: 'Bob'})
```

Incoming direction is also accepted:

```gql
INSERT (:User {name: 'Bob'})<-[:KNOWS]-(:User {name: 'Alice'})
```

### Delete

DELETE requires a preceding MATCH clause and a WHERE predicate.
Unbounded deletes (no WHERE) are rejected at validation time.

```gql
MATCH (a:User)-[:KNOWS]->(b:User)
WHERE b.name = 'spam'
DELETE b
```

An edge variable can also be the delete target:

```gql
MATCH (a)-[e:KNOWS]->(b)
WHERE b.name = 'spam'
DELETE e
```

## Supported WHERE operators

| Operator | Meaning               |
| -------- | --------------------- |
| `=`      | equal                 |
| `<>`     | not equal             |
| `<`      | less than             |
| `<=`     | less than or equal    |
| `>`      | greater than          |
| `>=`     | greater than or equal |

Multiple predicates are joined with `AND`.

## Supported value types

| Syntax           | Rust `Value` variant  |
| ---------------- | --------------------- |
| `42`             | `Value::Int(i64)`     |
| `3.14`           | `Value::Float(f64)`   |
| `'hello'`        | `Value::Text(String)` |
| `true` / `false` | `Value::Bool(bool)`   |
| `null`           | `Value::Null`         |

## Planner anchor heuristic

The planner selects a start node for the scan using the following priority:

1. **Property-equality anchor** — a node variable appearing in an `=` predicate against a literal.
2. **Label anchor** — the first node variable with a label constraint.
3. **Full scan** — the start node of the MATCH pattern (bounded by LIMIT when present).

## Phase 2 restrictions

The following features are intentionally unsupported and return
`GleaphError::UnsupportedFeature` at parse or validation time:

- `OPTIONAL MATCH`
- Variable-length paths (`[:REL*1..3]`)
- Aggregation functions (`COUNT`, etc.)
- Incoming-direction MATCH traversal (`<-`)
- More than 3 edge hops in a single MATCH
- Edge property hints in MATCH patterns

## Quick example

```rust
use gleaph_gql::{parse_statement, validate_statement};
use gleaph_gql::planner::build_plan;
use gleaph_gql::executor::execute_plan;
use gleaph_pma::{PmaGraph, VecMemory};

let mut graph = PmaGraph::new(VecMemory::default(), 0).unwrap();
// … populate graph …

let stmt = parse_statement("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name LIMIT 5").unwrap();
validate_statement(&stmt).unwrap();
let plan = build_plan(&stmt).unwrap();
let result = execute_plan(&plan, &graph).unwrap();

for row in result.rows {
    println!("{:?}", row);
}
```

## Execution limits

Use `execute_plan_with_limits` / `execute_mutation_with_limits` to cap
runaway queries:

```rust
use gleaph_gql::executor::{execute_plan_with_limits, ExecutionLimits};

let limits = ExecutionLimits {
    max_rows: Some(1_000),
    max_execution_steps: Some(100_000),
};
let result = execute_plan_with_limits(&plan, &graph, limits);
```

When a limit is exceeded the function returns `GleaphError::ExecutionError`.
