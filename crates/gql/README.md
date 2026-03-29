# gleaph-gql

An ISO/IEC 39075 (GQL) compliant graph query language parser written in Rust.

## Overview

Provides lexing, parsing, AST construction, type checking, and validation for queries based on the GQL standard.

## Features

- **Lexer / Parser** — Tokenization and parsing conforming to the GQL grammar
- **AST** — AST type definitions covering all sections of the GQL grammar
- **Type checking** — Verifies type consistency of queries
- **Validation** — Semantic-level verification
- **Temporal** — ISO 8601 compliant date/time parsing and formatting
- **Value / Comparison** — GQL standard scalar and constructed types with cross-width comparison

## Feature flags

| Flag         | Description                                      |
| ------------ | ------------------------------------------------ |
| `cypher`     | Cypher-compatible syntax extensions              |
| `sql-compat` | SQL-compatible syntax extensions                 |
| `f128`       | 128-bit float via `std::f128` (requires nightly) |
| `f256`       | 256-bit float via the `f256` crate               |

## Usage

```rust
use gleaph_gql::parser;
use gleaph_gql::validate::validate;

// Parse a GQL query into an AST
let program = parser::parse("MATCH (n:Person) RETURN n.name").unwrap();

// Validate the AST
validate(&program).unwrap();
```

Comments can also be preserved alongside the AST:

```rust
use gleaph_gql::parser;

let result = parser::parse_with_comments("/* find people */ MATCH (n:Person) RETURN n").unwrap();
println!("{:?}", result.comments);
```

## MSRV

Rust 1.88+ (edition 2024)

## License

MIT OR Apache-2.0
