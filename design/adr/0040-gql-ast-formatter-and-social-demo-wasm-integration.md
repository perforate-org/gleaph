# 0040. GQL AST formatter and social-demo WASM integration

Date: 2026-07-14
Status: Implemented
Last revised: 2026-07-14
Anchor timestamp: 2026-07-14 23:09:57 UTC +0000

## Context

The social demo's right-hand query panel should display the GQL query that is being
executed. It currently applies a small regular-expression-based formatter in
`frontend/apps/social-demo/src/components/QueryPanel.tsx`.

That formatter does not understand GQL structure. In particular, a `LIMIT` inside a
compound vector-search clause such as
`SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10)` can be mistaken for a
top-level clause and receive an incorrect line break. The same limitation applies to
other nested or clause-like syntax.

Query formatting is also not inherently a frontend concern. The formatted query may
eventually be displayed by the CLI, an administration console, query logs, or an
LSP-based editor integration. Maintaining separate formatters would duplicate GQL
knowledge and allow the output to diverge between products.

`gleaph-gql` is the portable ISO/IEC 39075 GQL parser crate. It already owns the AST
needed for this decision, including `SearchStatement` / `VectorSearchSpec`,
`ExprKind::ElementId`, and namespaced function calls such as `GLEAPH.SEQUENCE(e)`.
It currently has no AST formatter or `Display` implementation for reconstructing a
complete GQL program. The crate's existing features are `cypher`, `sql-compat`,
`f128`, `f256`, and `ast-rkyv-no-span`; its default features include `f128` and
`f256`. The social demo is a Vite + SolidJS + TypeScript application with no existing
Rust/WASM integration.

## Problem

The frontend formatter cannot reliably distinguish GQL grammar from text. Keeping
the formatting rule in the frontend also prevents the CLI, backend log views,
administration UI, and future LSP clients from sharing one source of truth.

The solution must provide structurally correct output for the social demo's five
fixed read queries while leaving room for broader GQL coverage and configurable
formatting. It must not add Gleaph storage, Router, Internet Computer, or canister
semantics to the portable GQL parser crate.

## Existing architecture assessment

The parser crate is the smallest existing owner of the relevant knowledge: token
structure, AST variants, identifier spelling, expressions, and query clauses. A
frontend-only formatter cannot own that knowledge without reimplementing a parser-like
model. The planner and Router are not suitable owners because formatting is a source
representation concern and does not participate in planning, authorization, routing,
or execution.

Adding the formatter to `gleaph-gql` therefore extends an existing language boundary
rather than creating a second query representation. The formatter will consume AST
values through public APIs and will not expose parser internals, storage state, or
execution plans.

The WASM transport is a separate integration concern. `gleaph-gql` remains usable from
native Rust and WASM without depending on a browser framework or requiring
`wasm-bindgen` in the core parser crate. A thin social-demo adapter will translate
TypeScript strings and formatter options to the Rust API and return either formatted
text or a structured error.

## Decision

### 1. Separate the formatter feature from the Gleaph dialect feature

Use one `gleaph` umbrella feature for Gleaph-specific syntax and AST forms. Do not
introduce individual crate features for `SEARCH`, `INLINE`, `IC`, or each future
extension unless a separate compilation or dependency boundary later justifies one.

The intended feature shape is:

```toml
[features]
default = ["f128", "f256"]
cypher = []
sql-compat = []
gleaph = []
format = []
```

`format` must not implicitly enable `gleaph`. A standard-GQL consumer can then use
`format` alone, while a Gleaph consumer such as the social demo explicitly selects
`format` and `gleaph`. During the implementation, existing Gleaph syntax that is
currently unconditional must either be gated under `gleaph` or be documented as a
compatibility migration prerequisite; adding a feature name without changing the
ownership boundary is not sufficient.

#### `gleaph` feature scope and consumer impact

`gleaph-gql` is consumed only by crates in this repository. The `gleaph` feature is
therefore non-default and must be selected explicitly by any crate that parses or
formats Gleaph-specific syntax such as `SEARCH ... VECTOR INDEX ...` or
`GLEAPH.SEQUENCE(e)`. Internal crates that currently rely on unconditionally-compiled
Gleaph AST forms must be updated to select the `gleaph` feature. External consumers
outside this repository are not affected because there are none.

### 2. Add a feature-gated formatter to `gleaph-gql`

Add a `format` Cargo feature and a `crates/gql/src/format.rs` module. Export the
module and its public API from `crates/gql/src/lib.rs` only when the feature is
enabled:

```rust
#[cfg(feature = "format")]
pub mod format;

#[cfg(feature = "format")]
pub use format::{format_program, format_query, FormatOptions};
```

The initial formatter supports the AST forms required by the social demo's five
fixed read queries and similar read queries, including graph patterns, `WHERE`,
`SEARCH` with vector-index specifications, distance/score aliases, `RETURN`, and
`ORDER BY`. Unsupported AST variants must fail explicitly through a formatter error;
they must not be silently dropped or printed as a misleading partial query.

`format_query` parses and formats a query. `format_program` formats an already parsed
program. Both receive `&FormatOptions`, so callers do not need a second API when they
later select a style or support range-oriented formatting.

Both functions return a single public `FormatError` result type. `format_query` maps
parser failures into the parse-error variant and reports unsupported AST forms through
the formatter-error variant; it must not fall back to regex formatting or return
partially formatted text. `format_program` can return formatter errors and option
validation errors but cannot produce a parse error because it receives an AST. The
error type must retain enough category information for the WASM adapter to distinguish
invalid query text from a valid but unsupported formatting shape.

The formatter is a source reconstruction facility, not a source-preservation
facility. Comments, insignificant original whitespace, and source spans are not
required to survive an AST round trip in this phase. Semantic values and clause
structure must be preserved, and formatting must never change query meaning.

### 3. Make formatting options an explicit, extensible value

`FormatOptions::default()` must produce the social-demo style with two spaces per
indentation level without requiring callers to construct a large parameter list. The
options type is the stable configuration boundary for future clients and may include,
as independently reviewable fields:

- an indentation unit supplied as an arbitrary non-empty string, such as `" "`,
  `"  "`, `"    "`, or `"\t"`; the formatter repeats this unit for each nesting level;
- a maximum preferred line width; when possible, the formatter wraps at clause or
  projection-item boundaries and does not split an individual expression or graph
  pattern;
- keyword casing: uppercase, lowercase, or preserve AST spelling where available;
- clause line-break policy;
- comma-after-break policy; and
- item-break policy for `RETURN` / `SELECT` projections.

Options describe presentation policy only. They must not encode Router, Graph, index,
canister, or product-specific behavior.

The indentation unit is not restricted to a fixed set of widths. The default is two
spaces, but callers may select any number of spaces or a tab without changing the
formatter API. The implementation may reject an empty indentation unit because it
would make nesting visually ambiguous; this is a formatting-option validation error,
not a GQL syntax error.

### 4. Integrate the formatter with the social demo through a thin WASM adapter

Build a small adapter for `frontend/apps/social-demo` that compiles the formatter
with the `format` feature and exposes a narrow string-in/string-out boundary to
TypeScript. The adapter owns JavaScript/WASM conversion, packaging, and generated
TypeScript declarations. It must not duplicate GQL formatting rules.

The existing regular-expression formatter in `QueryPanel.tsx` is removed after the
adapter is wired in. Query-panel rendering remains a UI responsibility; AST parsing
and formatting remain Rust responsibilities.

The WASM build must explicitly select compatible `gleaph-gql` features. The initial
implementation must verify whether the crate's default `f128` / `f256` features are
usable for the selected WASM target. If they are not, the adapter uses a documented
minimal feature set rather than changing the portable parser's defaults solely for the
demo. Any required build toolchain, generated bindings, and reproducible build steps
are part of the implementation plan and frontend package documentation.

### 5. Stage coverage instead of promising a complete pretty printer

Phase 1 is limited to the social-demo read-query surface and adjacent read forms.
DDL, DML, complete expression coverage, comment preservation, and LSP protocol or
range-formatting support are deferred. The public options boundary is designed for
those later uses, but deferred support must remain explicitly reported as unsupported.

### Extension formatting policy

Gleaph extensions are handled according to their AST representation:

- Extensions represented by generic AST forms, such as namespaced function and
  procedure calls including `GLEAPH.SEQUENCE(e)`, are formatted generically without
  extension-specific formatter branches.
- Extensions with dedicated AST forms, such as `SEARCH`, `DISTANCE AS`, `SCORE AS`,
  and future dedicated `INLINE` forms, are formatted structurally when `gleaph` is
  enabled. The formatter reconstructs their syntax but does not evaluate their
  execution meaning.
- Runtime or host extensions, such as `IC.PRINCIPAL` and caller-dependent functions,
  are emitted from their AST or extension representation. The formatter does not
  depend on principals, canisters, catalogs, or execution context.

The formatter does not validate index existence, metric compatibility, Router catalog
state, planner support, or Graph execution behavior. Those invariants remain owned by
the Router, planner, or execution integration. An unsupported dedicated AST form must
produce an explicit formatter error; it must never be silently omitted.

## Ownership and invariants

| Concern                      | Owner                                | Invariant                                                                     |
| ---------------------------- | ------------------------------------ | ----------------------------------------------------------------------------- |
| GQL syntax and AST traversal | `gleaph-gql::format`                 | Output is derived from the parsed AST and preserves supported query structure |
| Formatting policy            | `FormatOptions` and formatter module | Style choices do not contain product or execution semantics                   |
| WASM/TypeScript conversion   | social-demo adapter                  | The adapter forwards query text and options without reimplementing formatting |
| Query display                | `QueryPanel.tsx`                     | UI displays the formatter result and handles formatter errors explicitly      |
| Shared formatting behavior   | `gleaph-gql`                         | CLI, logs, admin UI, LSP, and demo can consume one formatter implementation   |

The AST in `gleaph-gql` is the canonical source of syntax structure. No frontend
regular-expression path remains authoritative after integration.

## Alternatives considered

### A. Keep formatting in the frontend

Continue repairing the regular-expression formatter. This has the lowest immediate
cost, but it cannot reliably model GQL nesting, duplicates GQL syntax knowledge, and
cannot serve backend, CLI, or LSP consumers. Rejected.

### B. Add a formatter to `gleaph-gql` for Rust consumers only

This centralizes syntax knowledge and provides native reuse, but leaves the social
demo with a separate formatter or requires it to display unformatted queries. It
solves only part of the consistency problem. Rejected as the final architecture.

### C. Add the formatter to `gleaph-gql` and call it from the frontend through WASM

This centralizes AST-based formatting and gives all current and future consumers a
shared implementation. It adds a WASM build and packaging pipeline, but that cost is
isolated in the integration adapter and is justified by the reuse and correctness
requirements. Accepted.

### D. Put browser bindings directly in `gleaph-gql`

This could reduce adapter code, but couples the general-purpose parser crate to a
browser binding technology and makes native, CLI, and LSP reuse less clean. Rejected;
the adapter boundary is intentionally separate.

## Consequences

Positive consequences:

- The social demo receives clause-aware formatting, including correct handling of
  nested vector-search `LIMIT` expressions.
- `gleaph-gql` becomes the single source of truth for supported GQL formatting.
- Native Rust consumers and future CLI, log, administration, and LSP integrations can
  reuse the same API.
- Formatting policy is explicit and can evolve without changing the basic formatting
  call shape.
- The portable GQL boundary remains free of Router, storage, ICP, and browser concerns.

Accepted costs and risks:

- Introducing `gleaph` as a real feature requires an audit of currently unconditional
  Gleaph syntax and explicit feature selection by existing consumers.
- The project gains a WASM build, generated bindings, and frontend packaging steps.
- The formatter must be expanded as new AST variants become required.
- AST reconstruction does not preserve comments or original source layout.
- WASM compatibility of `f128` / `f256` and the selected toolchain must be verified.
- Formatter error handling becomes part of the UI integration contract.

## Implementation plan and acceptance criteria

The implementation should proceed in bounded slices:

1. Define the `gleaph` feature boundary for dedicated Gleaph AST forms without
   changing unrelated default feature behavior.
2. Add `format`, `FormatOptions`, `FormatError`, and AST formatting for the Phase 1
   read-query subset; add unit tests for parser-error propagation, unsupported AST
   errors, each supported AST shape, and nested vector-search `LIMIT`.
3. Verify feature combinations, including `format` alone and `format,gleaph`, and
   resolve the WASM-compatible feature selection without changing unrelated defaults.
   Record the selected WASM target and exact feature set in the social-demo WASM build
   documentation.
4. Add the social-demo WASM adapter and generated TypeScript boundary; replace the
   regular-expression formatter in `QueryPanel.tsx`.
5. Build the Rust crate and frontend, then verify the five fixed social-demo queries
   render in the intended style in a browser.
6. Update README feature documentation and any affected GQL design documentation.

The phase is successful when:

- all five fixed social-demo queries are formatted structurally, including output of
  the following shape:

  ```text
  MATCH (p:Post)<-[:POSTED]-(author:User)
  WHERE p.is_public = TRUE
  SEARCH p IN (
    VECTOR INDEX post_vec
    FOR $query
    LIMIT 10
  ) DISTANCE AS distance
  RETURN
    p.demo_id AS post_id,
    author.name AS author_name,
    p.body AS body,
    distance
  ORDER BY distance ASC
  ```

- `cargo check -p gleaph-gql --features format` succeeds for standard formatting;
- `cargo check -p gleaph-gql --features format,gleaph` succeeds for Gleaph formatting;
- `gleaph-gql` also builds with formatting disabled;
- the social-demo WASM build documents the selected target and exact compatible
  `gleaph-gql` feature set, including whether `f128` and `f256` are enabled or omitted;
- the social-demo build succeeds and the browser displays formatted queries;
- formatter tests cover parser errors and unsupported forms and do not accept a
  silently truncated result; and
- callers can pass non-default `FormatOptions` values through the API, including a
  four-space indentation unit and a tab indentation unit.

## Design documentation impact

- Update `crates/gql/README.md` and the crate feature list in `crates/gql/src/lib.rs`,
  including the `gleaph` / `format` separation.
- Update [GQL stack layers](../gql/layers.md) to record the formatter as a portable
  parser-layer capability and the social-demo WASM adapter as an integration boundary.
- Document the frontend WASM build and generated binding workflow in the social-demo
  package as part of implementation.
- No planner, Router, Graph, storage, stable-memory, or canister design changes are
  implied by this ADR.

## Implementation status

**Implemented.** The `format` feature, public formatter API, AST-based read-query
formatter, explicit unsupported-shape errors, feature-focused tests, social-demo WASM
adapter, generated browser bindings, and QueryPanel integration are implemented. The
adapter selects `format,gleaph` with `default-features = false`, maps formatter error
categories across the JavaScript boundary, and fails closed in the query panel when
formatting is unavailable.

## Related documents

- [GQL stack layers](../gql/layers.md) — portable parser/planner/executor boundaries.
- [GQL extension syntax](../gql/extension-syntax.md) — syntax surface consumed by the
  parser and formatter as coverage expands.
