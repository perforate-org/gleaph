# 0034. Gleaph GQL extension syntax surface

Date: 2026-06-25
Status: accepted (syntax design; Rust manifest implemented; SEARCH parser/planner and Router lowering implemented, including Slice 6 leading labeled `SEARCH ... WHERE` equality filter, Slice 7 non-leading `SEARCH ... WHERE` equality filter, Slice 8 one or two `AND`-connected same-binding equality conjuncts, and Slice 9 one same-binding numeric range predicate for both leading and non-leading search; remaining syntax staged by feature)
Last revised: 2026-06-29
Anchor timestamp: 2026-06-29 14:50:26 UTC +0000

> **Summary.** Gleaph needs a coherent public GQL dialect surface for IC values, graph-local inline
> edge data, vector search, shortest-path costs, and operational procedures. This ADR accepts a
> separate dialect contract under `design/gql/extension-syntax.md` instead of folding each syntax
> decision into feature-specific ADRs. The rule is: daily query syntax should be declarative and close
> to ordinary GQL; operational procedures remain under `GLEAPH.*`; implementation details such as
> vector canister routing, payload byte profiles, and maintenance APIs must not leak into the public
> query language.

## Context

Gleaph already has several GQL-adjacent extensions:

- `IC.PRINCIPAL` values in `gleaph-gql-ic`.
- `MSG_CALLER()` as an IC runtime function in graph execution.
- `GLEAPH.WEIGHT(e)` and `GLEAPH.COST BY ...` for edge payload weights and shortest-path costs.
- `GLEAPH.SEQUENCE(e)` for Graph-owned edge insertion-order compensation in `ORDER BY`.
- `GLEAPH.VECTOR.*` fused edge-payload vector predicates.
- `CALL GLEAPH.FINALIZE_*` / `CALL GLEAPH.DRAIN_DEFERRED_MAINTENANCE()` for operational mutation
  procedures.
- ADR 0031 direct Router/vector-canister `vector_search`, not exposed through GQL syntax.

These decisions landed incrementally. Without a single dialect contract, future syntax could drift:
vertex embedding search might be exposed as a procedure, edge payload bytes might keep leaking through
`GLEAPH.WEIGHT`, and IC-specific concepts might enter the generic `gleaph-gql` crates.

External syntax direction has also moved. Grafeo-style examples present vector similarity as part of
the graph query rather than as an out-of-band API. Neo4j Cypher 25 introduced a `SEARCH` subclause for
vector indexes (`MATCH ... SEARCH variable IN (VECTOR INDEX ... FOR ... LIMIT ...) SCORE AS ...`) and
deprecated the older vector query procedures. Gleaph should not copy another system blindly, but this
confirms that first-class search syntax is a better public shape than `CALL GLEAPH.VECTOR_SEARCH`.

## Problem

Feature-specific ADRs answer the storage and canister questions, but they do not define the user-facing
Gleaph GQL dialect as a whole. The missing contract creates several risks:

| Risk                                                                             | Impact                                                                      |
| -------------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| Procedure-shaped vector search becomes the public API                            | Harder to compose with `MATCH`, traversal, `WHERE`, and ranking             |
| `GLEAPH.WEIGHT` / `GLEAPH.PAYLOAD` stay as daily query syntax                    | Edge payload storage details remain visible to users                        |
| `GLEAPH.VECTOR.*` is reused ambiguously                                          | Edge-payload vector predicates and vertex embedding search become conflated |
| IC/runtime extensions are documented separately from search/traversal extensions | No single place explains what is part of the Gleaph dialect                 |
| Gleaph-specific syntax lands in `gleaph-gql` without a boundary rule             | Portable GQL crates become coupled to Gleaph storage/canister concepts      |

## Existing Architecture Assessment

The existing crate boundaries are still the right foundation:

- `gleaph-gql` owns generic parsing, AST, validation, and extension value mechanics.
- `gleaph-gql-planner` owns generic physical plan shapes and extension hooks, but must remain free of
  GraphStore, stable memory, vector canisters, shard ids, and IC canister assumptions.
- `gleaph-router` owns graph context, catalog/index definition resolution, authorization, query
  orchestration, and vector-index target resolution.
- `gleaph-graph` owns shard-local graph execution, inline edge payload decoding, and runtime functions
  that need caller/execution context.
- `graph-vector-index` owns ANN search, vector maintenance, rebuilds, and ranking internals.

Therefore the syntax contract should be explicit, but most implementation should continue to live in
Router/Graph integration layers. Only syntax that is intentionally part of the Gleaph dialect should
enter the parser; backend-specific meaning must be attached later by the owning domain.

## Alternatives

### A. Keep extending feature ADRs only

Document vector search in ADR 0031, edge payload syntax in ADR 0008, IC values in `gql-ic`, and
operational procedures near bulk-ingest code.

- Benefits: no new document.
- Drawbacks: no coherent dialect policy; repeated namespace and boundary decisions; easy to expose
  implementation-shaped APIs as public syntax.

### B. Use only standard `CALL ... YIELD` for every extension

Expose vector search, maintenance, payload reads, and runtime operations as `CALL GLEAPH.*`.

- Benefits: minimum parser work; follows existing procedure infrastructure.
- Drawbacks: poor readability for daily search/traversal queries; vector search becomes less
  composable; conflicts with the direction of graph-native vector search syntax.

### C. Create a dedicated Gleaph GQL extension syntax contract

Keep generic GQL crates portable, but document the Gleaph dialect surface as a coherent layer:
`INLINE`, `SEARCH`, `SCORE/DISTANCE`, `COST BY`, `IC.PRINCIPAL` / `MSG_CALLER`, and operational
`GLEAPH.*` procedures.

- Benefits: clear public syntax direction; separates declarative query syntax from operational
  procedures; aligns vector search with graph query composition; names ownership boundaries.
- Drawbacks: requires a new design document and future parser/planner work for syntax not yet
  implemented.

## Decision

Adopt **Alternative C**.

Create `design/gql/extension-syntax.md` as the steady-state syntax contract for Gleaph's GQL dialect.
This ADR records why that contract exists and the top-level policy:

1. **Daily graph-query syntax should be declarative.** Vector search is a first-class `SEARCH`
   subclause, not a public `CALL GLEAPH.VECTOR_SEARCH(...)` procedure.
2. **Edge-local fast values are ordinary property access with a schema/storage modifier.** New syntax
   should prefer `e.distance`, `e.score`, or `e.stats.confidence` over `GLEAPH.WEIGHT(e)` /
   `GLEAPH.PAYLOAD(e)`.
3. **Embeddings are not inline properties.** Vertex embeddings belong to the canonical embedding store
   and derived vector-index model, not to edge payload storage and not to ordinary variable-size
   property payloads.
4. **Operational procedures stay under `GLEAPH.*`.** Maintenance, finalize, backfill, and internal
   imperative operations remain explicit procedures.
5. **IC extensions are part of the dialect but not portable GQL.** `IC.PRINCIPAL` and `MSG_CALLER()`
   stay in bridge/execution layers and must not turn `gleaph-gql` into an IC-dependent crate.
6. **Parser additions are allowed only for first-class dialect features.** Internal execution concepts
   must use existing extension hooks, Router recognition, or Graph execution context instead of
   leaking into generic GQL grammar.
7. **Rust must have a canonical extension manifest.** Gleaph-specific names must not remain scattered
   as ad hoc string literals. A pure Rust manifest should record the canonical name, syntax class,
   implementation status, owner, and design-document anchor for each dialect extension. The manifest
   is a registry and recognizer layer, not an execution dispatcher: Router, Graph, planner
   integration, `gleaph-gql-ic`, and the vector-index canister still own their respective semantics.

## Consequences

- ADR 0031 can keep focusing on vector-index storage, sync, rebuild, and maintenance. The GQL syntax
  for using vector search is governed by this ADR and `design/gql/extension-syntax.md`.
- The long-term public vector syntax is:

  ```gql
  MATCH (d:Document)
    SEARCH d IN (
      VECTOR INDEX document_embedding
      FOR $query
      LIMIT 100
    ) SCORE AS similarity
  RETURN d, similarity
  ```

- The implementation may still lower this to the existing Router/vector-canister `vector_search`
  API. That lowering is internal, not the public GQL contract.
- Existing `GLEAPH.WEIGHT`, `GLEAPH.SEQUENCE`, `GLEAPH.COST`, and `GLEAPH.VECTOR.*` remain valid
  implementation-era surfaces until migration syntax lands; the new document marks their target
  status explicitly.
- Existing and planned extension names should be centralized in a pure Rust manifest before adding
  more syntax. The manifest should be dependency-light and contain descriptors/recognizers such as
  value types, runtime functions, path extensions, edge-payload vector predicates, search clauses,
  schema modifiers, and operational procedures. It must not call the Router, Graph, stable-memory
  stores, or vector-index canisters.

## Trade-offs

- A first-class `SEARCH` subclause is more parser/planner work than `CALL ... YIELD`.
- The syntax must be staged carefully to avoid adding Gleaph-specific backend meaning to
  `gleaph-gql` or `gleaph-gql-planner`.
- `SCORE AS` vs `DISTANCE AS` needs metric-specific semantics. Similarity metrics naturally produce a
  score where higher is better; distance metrics naturally produce a distance where lower is better.
  The syntax contract allows both names but requires each vector-index definition to expose only the
  scoring shape it can define honestly.

## Migration

No immediate code or stable-memory migration.

Planned migration path:

1. Document existing extensions and target syntax in `design/gql/extension-syntax.md` (done).
2. Add the Rust extension manifest in `gleaph-graph-kernel::gql_dialect` without changing behavior (done):
   - represent canonical names such as `IC.PRINCIPAL`, `MSG_CALLER`, `GLEAPH.COST`,
     `GLEAPH.WEIGHT`, `GLEAPH.SEQUENCE`, `GLEAPH.VECTOR.*`, and `GLEAPH.FINALIZE_*`;
   - classify planned syntax such as `SEARCH`, `INLINE`, and `CREATE VECTOR INDEX`;
   - expose exact and case-insensitive recognizers for owners that already parse extension names;
   - add tests that implemented Gleaph extension entry points are registered in the manifest.
3. Replace scattered hard-coded Gleaph extension names with manifest helpers where this does not
   change behavior (done).
4. Keep existing `GLEAPH.WEIGHT` / `GLEAPH.VECTOR.*` behavior while adding ordinary-property inline
   syntax in schema/planner/executor slices.
5. Add `SEARCH` parser/planner support as a Gleaph dialect feature (done). Router lowering to the existing vector search API is implemented for the narrow leading `NodeScan + Search` prefix and for one top-level non-leading `SEARCH` after a bound vertex, vertex-only. `DISTANCE AS` is accepted for distance-only metrics and `SCORE AS` is accepted for exact-scan cosine indexes (`nlist == 1`); `SCORE AS` is rejected for metrics that have no natural score (e.g. `L2Squared`). Cosine partition-page scan (`nlist > 1`) is fail-closed in the vector canister in this slice. Non-leading `SEARCH` semantics are exactly `input rows INNER JOIN global vector top-k` on the bound vertex; vector search runs once per query, global top-k is computed before the join, and row multiplicity is preserved. Correlated/per-row `FOR`/`LIMIT`, nested/multiple search, and edge subjects remain planned.
6. Add leading labeled `SEARCH ... WHERE` equality filter (ADR 0034 Slice 6, done). The planner accepts one same-binding property equality predicate (`d.category = $category` or `$category = d.category`) and carries it in `PlanOp::Search`; the Router proves exact label/property index coverage, resolves a bounded candidate allowlist from the Property Index, and asks Vector Index to rank exactly within that set. Empty candidates preserve the leading-search aggregate dispatch contract; candidate sets larger than 4096 fail explicitly.
7. Add non-leading labeled `SEARCH ... WHERE` equality filter (ADR 0034 Slice 7, done). The planner now fuses the destination-node label into `ExpandFilter.dst_filter` so the Router can prove the searched label from the prefix. For any non-leading `SEARCH` with a filter, the Router requires exactly one positive simple label proof from the top-level prefix (`NodeScan` or `PropertyFilter`/`ExpandFilter` `IS LABELED`), reuses the same bounded Property Index candidate resolution as Slice 6, and dispatches an explicit empty resolved-search relation when candidates are empty so Graph still executes the prefix and global aggregates return one zero row. The relational contract remains one global filtered top-k before the prefix inner join; the result may contain fewer than `LIMIT` rows and one hit may produce multiple output rows.
8. Add exactly two `AND`-connected same-binding equality conjuncts for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 8, done).
9. Add one same-binding numeric range predicate (`<`, `<=`, `>`, `>=`) for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 9, done). The planner accepts a single range comparison between a property of the searched binding and a literal or parameter; the Router resolves the property, proves an active vertex property index for the same `(graph_id, label_id, property_id)` tuple, derives a finite half-open encoded numeric range through the canonical `gleaph_gql::numeric_range_bounds` helper, and collects label-qualified candidates through the bounded paginated `lookup_range_page` path. The candidate set is the exact label-scoped numeric range before Vector Index ranking; out-of-range and non-numeric values cannot consume top-k positions. Empty ranges preserve the leading/non-leading aggregate dispatch contract. The planner accepts one equality or exactly two `AND`-connected equalities on distinct properties of the searched binding and carries the original filter expression in `PlanOp::Search`; the Router proves active vertex equality index coverage for every arm, resolves each literal/parameter value, and collects candidates through the existing server-side `lookup_intersection_page` for two-arm filters or `lookup_equal_page` for single-arm filters. The candidate set is the exact label-scoped Property Index intersection, bounded to 4096 distinct subjects, before Vector Index ranking. Empty intersections skip the vector canister and preserve the leading/non-leading aggregate dispatch contract from Slices 6 and 7.
10. Mark procedure-shaped vector search as internal/escape-hatch only if it is ever added.

## Design Documentation Impact

- Add `design/gql/extension-syntax.md` (done).
- Link the new document from `design/gql/layers.md` (done).
- Link ADR 0034 from `design/adr/README.md` (done).
- Add the Rust extension manifest in `gleaph-graph-kernel::gql_dialect` (done). Update this ADR and
  `design/gql/extension-syntax.md` if the module is extracted into a dedicated crate or if its
  location otherwise changes the boundary model.
- Future implementation slices must update `design/gql/extension-syntax.md` when a planned syntax
  becomes implemented.

## Required Axes Impact

- **Encapsulation:** preserved. Storage layout and canister details stay behind Router/Graph/Index APIs.
- **Separation of concerns:** strengthened. Generic GQL crates keep parsing/language mechanics; Gleaph
  integration layers own backend meaning.
- **Invariants:** clarified. Inline fields, embeddings, vector indexes, and operational procedures each
  have a named owner.
- **Consistency:** strengthened. There is one dialect document and, once implemented, one Rust
  manifest for extension names and classification instead of scattered syntax decisions.
- **Fitness for purpose:** the contract is broad enough to cover known Gleaph extensions without
  turning into a generic plugin framework.
