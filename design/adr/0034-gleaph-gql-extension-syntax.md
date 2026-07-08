# 0034. Gleaph GQL extension syntax surface

Date: 2026-06-25
Status: accepted (syntax design; Rust manifest implemented; SEARCH parser/planner and Router lowering implemented, including Slice 6 leading labeled `SEARCH ... WHERE` equality filter, Slice 7 non-leading `SEARCH ... WHERE` equality filter, Slice 8 one or two `AND`-connected same-binding equality conjuncts, Slice 9 one same-binding numeric range predicate for both leading and non-leading search, Slice 10 exactly two same-property range predicates (one lower, one upper) forming a two-sided numeric range, Slice 11 one equality predicate plus one one-sided numeric range predicate on distinct properties, Slice 12 one equality predicate plus two same-property range predicates (one lower and one upper) on a distinct property, Slice 13 bounded N-way (1..=8) same-binding equality conjunctions with provider-neutral Planner syntax, Slice 14 one to eight equality predicates plus one one- or two-sided numeric range predicate on a distinct property, Slice 15 bounded same-property equality disjunctions (2..=8 OR-connected arms) with provider-neutral Planner syntax and Router-owned union execution, Slice 16 bounded cross-property pure equality disjunctions (2..=8 OR-connected arms) with provider-neutral Planner syntax and Router-owned union execution, Slice 17 bounded same-property numeric range disjunctions (2..=8 OR-connected arms) with provider-neutral Planner syntax and Router-owned union execution, Slice 18 bounded cross-property numeric range disjunctions (2..=8 OR-connected arms) with provider-neutral Planner syntax and Router-owned per-property normalization and union execution, Slice 19 bounded heterogeneous equality/range disjunctions (2..=8 OR-connected arms, each leaf independently equality or one-sided numeric range) with provider-neutral Planner syntax and Router-owned per-property normalization and union execution; Slice 20 scalar `INLINE` edge-property schema registration, Slice 21 ordinary read access to that inline scalar, Slice 22 ordinary mutation packing of scalar values into the inline payload, Slice 23 ordinary `COST BY e.<inline-property>` shortest-path cost, and Slice 24 fixed-size inline edge struct schema registration implemented; Slice 25 ordinary read access to fixed-size inline edge struct fields implemented; struct mutation packing, `COST BY` over a struct field, generic `CREATE GRAPH TYPE` `INLINE` annotations, and vector-index DDL remain planned)
Last updated: 2026-07-03
Anchor timestamp: 2026-07-03 09:54:43 UTC +0000

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
- `GLEAPH.WEIGHT(e)` and `GLEAPH.COST BY ...` for edge inline value weights and shortest-path costs.
- `GLEAPH.SEQUENCE(e)` for Graph-owned edge insertion-order compensation in `ORDER BY`.
- `GLEAPH.VECTOR.*` fused edge-inline-value vector predicates.
- `CALL GLEAPH.FINALIZE_*` / `CALL GLEAPH.DRAIN_DEFERRED_MAINTENANCE()` for operational mutation
  procedures.
- ADR 0031 direct Router/vector-canister `vector_search`, not exposed through GQL syntax.

These decisions landed incrementally. Without a single dialect contract, future syntax could drift:
vertex embedding search might be exposed as a procedure, edge inline value bytes might keep leaking through
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
- `gleaph-graph` owns shard-local graph execution, inline edge inline value decoding, and runtime functions
  that need caller/execution context.
- `graph-vector-index` owns ANN search, vector maintenance, rebuilds, and ranking internals.

Therefore the syntax contract should be explicit, but most implementation should continue to live in
Router/Graph integration layers. Only syntax that is intentionally part of the Gleaph dialect should
enter the parser; backend-specific meaning must be attached later by the owning domain.

## Alternatives

### A. Keep extending feature ADRs only

Document vector search in ADR 0031, edge inline value syntax in ADR 0008, IC values in `gql-ic`, and
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
   and derived vector-index model, not to edge inline value storage and not to ordinary variable-size
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
  value types, runtime functions, path extensions, edge-inline-value vector predicates, search clauses,
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
9. Add one same-binding numeric range predicate (`<`, `<=`, `>`, `>=`) for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 9, done). The planner accepts a single range comparison between a property of the searched binding and a literal or parameter; the Router resolves the property, proves an active vertex property index for the same `(graph_id, label_id, property_id)` tuple, derives a finite half-open encoded numeric range through the canonical `gleaph_gql::numeric_range_bounds` helper, and collects label-qualified candidates through the bounded paginated `lookup_range_page` path. The candidate set is the exact label-scoped numeric range before Vector Index ranking; out-of-range and non-numeric values cannot consume top-k positions. Empty ranges preserve the leading/non-leading aggregate dispatch contract.
10. Add exactly two same-binding range predicates on the same property of the searched binding for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 10, done). The planner accepts exactly one lower bound (`>` or `>=`) and one upper bound (`<` or `<=`) on the same property, with either conjunct order and either operand order; it rejects equality-plus-range, different properties, duplicate directions, three or more predicates, and computed operands. The Router resolves the property once, proves one active vertex property index, derives each arm's finite half-open encoded interval through `gleaph_gql::numeric_range_bounds`, intersects the two intervals (`low = max(first.low, second.low)`, `high = min(first.high, second.high)`), and issues at most one paginated `lookup_range_page` stream with `PostingRangeRequest::Between { low, high }`. If `low >= high` the candidate set is empty before any Property Index or Vector Index call; the leading/non-leading empty-candidate dispatch contract is preserved. Out-of-range values and non-numeric values cannot consume top-k positions.
11. Add one to eight equality predicates and one one- or two-sided numeric range predicate on a distinct property of the searched vertex for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slices 11 and 14, done).
12. Add exactly two same-property numeric range predicates (one lower and one upper) on a distinct property of the searched vertex, optionally combined with one to eight equality predicates on other distinct properties, for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slices 12 and 14, done).
13. Add bounded N-way (1..=8) same-binding equality conjunctions for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 13, done). The Planner accepts any number of `AND`-connected equality predicates on the searched binding, as long as each refers to a distinct property of that binding and no mixed range or duplicate-property arms are present. The provider-neutral contract is therefore "arbitrary-length pure equality conjunction on distinct properties". The Router / Property Index implement a shared execution bound `MAX_EQUALITY_INTERSECTION_ARMS = 8` in `gleaph-graph-kernel`; the Router dispatches one arm through `lookup_equal_page`, two to eight arms through `lookup_intersection_page`, and rejects nine or more arms with `InvalidArgument`. The Property Index enforces the same 2..=8 spec range in `lookup_intersection_page`, canonicalises the walk arm by `(property_id, encoded_value)` order for deterministic paging, and materialises all sieve arms server-side per page. Empty candidate sets preserve the existing leading/non-leading aggregate dispatch contract. This is the first slice that decouples provider-neutral Planner acceptance from the bounded, canister-facing execution primitive; it does not introduce new stable memory layout, new index types, or Vector Index behavior.
14. Add one or more equality predicates plus one one- or two-sided numeric range predicate on a distinct property for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 14, done). The Planner accepts an arbitrary number of `AND`-connected equality predicates on distinct properties of the searched binding together with one numeric range dimension (one lower bound, one upper bound, or one of each on the same property). The range property must differ from every equality property. The Router / Property Index execute one through eight equality arms combined with a single finite encoded numeric range through `lookup_range_intersection_page`; nine or more equality arms are rejected before any Property Index or Vector Index call. The Property Index walks the finite range, applies each equality sieve sequentially to the current page, and preserves the range cursor even when a page has no survivors. Empty candidate sets and empty ranges preserve the existing leading/non-leading aggregate dispatch contract. This slice generalizes Slices 11 and 12 and reuses the same `MAX_EQUALITY_INTERSECTION_ARMS` source of truth; it does not introduce a second mixed-filter limit, a new endpoint, or Vector Index behavior changes.
15. Add bounded same-property equality disjunctions (2..=8 OR-connected arms) for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 15, done). The Planner accepts any number of `OR`-connected equality predicates on the searched binding, as long as every arm compares the same property of that binding to a literal or parameter, and no arm contains a range, a different property, or a nested logical operator. The provider-neutral contract is therefore "arbitrary-length pure same-property equality disjunction". The Router classifies this shape as a first-class `EqualityDisjunction` filter, enforces a separate Router-owned execution bound of eight arms, requires one active vertex property index for the shared `(graph_id, label_id, property_id)` tuple, and resolves the candidate set by walking each index source and each encoded value sequentially. For every `(index_source, encoded_value)` pair the Router issues paginated `lookup_equal_page` calls, label-filters each page before counting, and merges results with global deduplication by `(shard_id, vertex_id)`. It stops as soon as the 4096 candidate bound is exceeded and returns an explicit error instead of truncating. A single equality predicate remains the existing `Equality` path; mixed OR/AND, OR across different properties, range inside OR, and nine or more arms fail closed. Empty candidate sets preserve the existing leading/non-leading aggregate dispatch contract. This slice does not change the Property Index endpoint contract or Vector Index behavior; the union logic lives entirely in the Router.
16. Add bounded cross-property pure equality disjunctions (2..=8 OR-connected arms) for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 16, done). The Planner accepts any number of `OR`-connected equality predicates on the searched binding, as long as every arm is a pure equality comparison between a property of that binding and a literal or parameter, and no arm contains a range, a different binding, or a nested logical operator. The provider-neutral contract is therefore "arbitrary-length pure equality disjunction on the same binding". The Router generalizes the Slice 15 `EqualityDisjunction` path: each arm is resolved to its own `(graph_id, label_id, property_id)` tuple, each tuple must have an active vertex property index, and the candidate set is built by walking every distinct `(property_id, encoded_value)` source sequentially. The same 2..=8 arm bound, 4096 candidate bound, per-page label filtering, and global `(shard_id, vertex_id)` deduplication are preserved. Repeated properties across arms are accepted; duplicate `(property_id, encoded_value)` sources are merged into a single lookup so the same source is not read twice. Mixed OR/AND, range inside OR, a different binding in any arm, and nine or more arms remain fail-closed. Empty candidate sets preserve the existing leading/non-leading aggregate dispatch contract. The union logic continues to live entirely in the Router; neither the Property Index endpoint contract nor the Vector Index behavior changes.
17. Add bounded same-property numeric range disjunctions (2..=8 OR-connected arms) for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 17, done). The Planner accepts any number of `OR`-connected range predicates on the searched binding as long as every arm is a pure numeric range comparison (`<`, `<=`, `>`, `>=`) between the same property of that binding and a literal or parameter, with no equality, no different property, and no nested logical operator. The provider-neutral contract is therefore "arbitrary-length pure same-property numeric range disjunction". The Router resolves the shared property once, proves a single active vertex property index, derives a finite half-open encoded interval per arm using the canonical `gleaph_gql::numeric_range_bounds` helper, drops empty or contradictory intervals, merges overlapping/touching intervals, enforces the 2..=8 arm bound, and executes the union through the shared `lookup_range_page` candidate collector. Empty candidate sets preserve the normal search aggregate contract.
18. Add bounded cross-property numeric range disjunctions (2..=8 OR-connected arms) for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 18, done). The Planner accepts any number of `OR`-connected range predicates on the searched binding as long as every arm is a pure numeric range comparison (`<`, `<=`, `>`, `>=`) between a property of that binding and a literal or parameter, with no equality and no nested logical operator; arms may reference the same property or different properties. The provider-neutral contract is therefore "arbitrary-length pure same-binding numeric range disjunction". The Router resolves every arm to its own property id, proves an active vertex property index **per property**, derives a finite half-open encoded interval per arm, drops empty or contradictory intervals, merges overlapping/touching intervals **within each property id**, enforces the 2..=8 syntactic arm bound, and executes the union through the shared `lookup_range_page` candidate collector. Intervals are not merged across property ids because encoded numeric keys are property-specific.
19. Add bounded heterogeneous equality/range disjunctions (2..=8 OR-connected arms) for leading and non-leading `SEARCH ... WHERE` (ADR 0034 Slice 19, done). The Planner accepts any number of `OR`-connected comparison predicates on the searched binding as long as every arm is independently either an equality comparison (`=`) or a one-sided numeric range comparison (`<`, `<=`, `>`, `>=`) between a property of that binding and a literal or parameter, with no nested logical operator, no two-sided range disjunct, and no non-comparison leaf. Properties may repeat or differ across arms and across comparison kinds. The provider-neutral contract is therefore "arbitrary-length same-binding comparison disjunction". The Router enforces one 2..=8 syntactic arm bound, resolves every arm to its own property id, proves an active vertex property index **per arm**, encodes equality values, derives finite half-open encoded intervals for range arms, validates all encoded key sizes, deduplicates exact `(property_id, encoded_value)` equality sources, groups range intervals by property id and merges overlapping/touching intervals **within each property id**, and executes the union of normalized equality and range sources through the shared bounded candidate collector. Equality and range sources may target the same property; they are not merged with each other because they are semantically distinct postings lookups. Intervals are not merged across property ids.

20. Add scalar `INLINE` edge-property schema registration as a Router-owned DDL path (ADR 0034 Slice 20, implemented). The standalone statement `CREATE EDGE LABEL <label> { <property> <fixed_scalar_type> INLINE }` is parsed at the Router dialect boundary, interned through the existing label/property catalogs, and persisted as a versioned `EdgeInlineValueSchemaRecord` in `ROUTER_EDGE_PAYLOAD_PROFILES`. The physical `EdgeInlineValueProfile` is derived from the scalar type and travels on the existing `ResolvedEdgeLabel` wire to Graph, so payload DML/execution consumes the declared width/encoding without any Graph-side schema ownership change. Exactly one inline slot per edge label is enforced; exact replay is idempotent; conflicting declarations return `Conflict` with no partial catalog mutation; the `UnnamedProfile` variant remains for admin-installed profiles and cannot be silently overridden by the admin setter.

21. Add ordinary read access to a Slice 20 scalar inline property through standard `e.property` syntax (ADR 0034 Slice 21, implemented). Router projects the scalar inline schema (`ResolvedInlineSchema::Scalar { property_id }`) onto `ResolvedEdgeLabel.inline_schema`; Graph resolves the named property through `ResolvedPropertyTable`, matches it against the concrete edge label's inline slot, and strictly decodes the bound edge inline value bytes into the exact GQL scalar value. For the matching inline property, payload bytes are the only read source; malformed or missing payloads fail closed and a sidecar property value cannot override or rescue the read. Projection, `WHERE`, comparisons, aggregate inputs, and `ORDER BY` reuse one shared inline-aware read helper. Until an explicit inline-index maintenance slice exists, creating or activating an edge Property Index for the same `(label_id, property_id)` is rejected in both DDL orders.

22. Add ordinary mutation packing of scalar values into a Slice 20 inline payload through standard GQL edge mutations (ADR 0034 Slice 22, implemented). For a concrete Router-resolved edge label with an `InlineScalar` schema, `INSERT` requires exactly one assignment for the named inline property, evaluates and validates it before creating any adjacency record, encodes it into the fixed-width payload bytes, and inserts the edge through the existing directed/undirected payload-aware path. `SET e.inline_property = <expr>` and `SET e = { ... }` re-resolve the concrete label, require and encode the inline value exactly once, and update the payload through the existing mirrored forward/reverse/undirected commit. `REMOVE e.inline_property` is rejected because this slice has no absence representation. Non-inline properties on the same edge retain existing sidecar storage and index-maintenance behavior. Graph uses one shared scalar codec for encoding (mutation), decoding (read), and raw predicate-byte preparation; no second schema table or sidecar fallback is introduced. Invalid, missing, duplicate, or `NULL` inline values fail closed before any canonical write.

23. Add ordinary `COST BY e.<inline-property>` shortest-path cost for a scalar `INLINE` edge property (ADR 0034 Slice 23, implemented). The bounded shape requires a shortest-path pattern with exactly one extension clause, a single concrete edge label, one declared edge variable, and a direct property access whose base is that edge variable. The Graph planner integration recognizes unqualified `COST BY` separately from the compatibility surface `GLEAPH.COST BY ...`; generic physical-plan property-use collection now includes properties referenced inside `PlanOp::ShortestPath::EdgeCostExpr`; and weighted hop evaluation receives the Router-resolved label and property tables. Graph resolves the property through `ResolvedPropertyTable`, proves it equals the scalar inline property id in the concrete label's `inline_schema`, and evaluates each hop through the shared inline-aware edge property reader. Validation, ordering, accumulation, finite/non-negative checks, and overflow handling remain owned by `WeightedCost`. Existing `GLEAPH.COST BY GLEAPH.WEIGHT(e)` behavior and its direct payload-decoder fast path are preserved.

Slice 25 implements ordinary read access to Slice 24 fixed-size inline structs: `e.stats` returns a
GQL record and `e.stats.score` works in projection, `WHERE`, comparisons, aggregate inputs, and
`ORDER BY`. Router derives a bounded physical field projection from the canonical declaration; Graph
validates the projection against the payload width and decodes the payload bytes into a
declaration-ordered `Value::Record`. Struct mutation packing, `COST BY` over a struct field,
property indexes on inline struct fields, nested structs, and generic `CREATE GRAPH TYPE` `INLINE`
annotations remain planned.

> Procedure-shaped vector search remains an internal/escape-hatch consideration only if a concrete
> operational need appears; it is not assigned a slice number.

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
