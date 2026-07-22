# 0046. Multi-variable candidate seed relations with canonical Graph revalidation

Date: 2026-07-21
Status: Proposed
Last revised: 2026-07-21
Anchor timestamp: 2026-07-21 01:42:34 UTC +0000

## Context

Federated GQL execution uses Router-resolved index or label hits to select Graph shards and seed the
leading read prefix. The current `SeedAnchorSet` represents one variable with one or more restrictions.
If a leading prefix binds more than one variable, the Router deliberately returns no partial seed so
Graph does not receive a row that binds only one endpoint.

That fail-safe behavior is correct but inefficient for shapes such as:

```gql
MATCH (a:Post {demo_id: $a_demo_id, demo_graph: 'social'}),
      (b:Post {demo_id: $b_demo_id, demo_graph: 'social'})
RETURN a NEXT
INSERT (a)-[:REPLY_TO {demo_edge_id: $edge_id, demo_kind: $demo_kind}]->(b)
```

Both equality anchors may have selective Property Index access paths, but Graph performs the full
read prefix when the Router cannot represent both bindings. ADR 0044 bulk grouping magnifies the
cost: one plan and one `MutationId` are shared by many parameter sets, while every item may need a
different pair of endpoint seeds.

The existing wire has two unversioned shapes inside `SeedBindingsWire`:

- `entries`: grouped hits for one variable;
- `rows`: complete `SeedRowWire` values, introduced for leading `SEARCH` lowering.

Complete rows can represent `(a, b)`, but materializing every Cartesian-product row in Router makes
wire size and Router memory `O(product(cardinality))`. Reinterpreting several legacy `entries` as a
Cartesian product would be ambiguous and would silently change existing single-variable semantics.

The current Graph hydration checks vertex existence, tombstones, and required labels. Seeded
execution then skips leading `NodeScan` / `IndexScan` / `IndexIntersection` / `EdgeIndexScan`
operators. It does not re-evaluate every canonical property equality represented by a skipped
index operator. A stale derived-index hit therefore cannot by itself be treated as proof that the
canonical Graph row still matches.

ADR 0029 establishes the governing boundary: Router may fetch remote inputs before dispatch, but
Graph owns canonical graph state and must revalidate owner-controlled preconditions before the
no-`await` canonical mutation segment. Property and label postings remain derived state, not a
participant in Graph's local atomic commit.

## Problem

Provide one general execution contract that:

1. resolves several independent, parameterized equality/label anchors without falling back to one
   Graph full scan per bulk item;
2. preserves bulk plan reuse, Graph batch dispatch, item result order, and idempotent replay;
3. does not materialize an unbounded Cartesian product in Router or on the wire;
4. keeps Graph as the final authority for canonical predicate and label membership;
5. uses declared constraints when they provide a stronger bounded lookup without making the general
   path depend on application-level uniqueness; and
6. fails closed when candidate, product, payload, consistency, or instruction bounds cannot be met.

## Existing architecture assessment

The existing ownership domains are sufficient; no new canister or query language feature is needed.

- Router owns plan inspection, parameter decoding, index/constraint catalogs, shard routing,
  Property Index orchestration, the immutable mutation envelope, and bulk result mapping.
- Property Index owns derived posting lookup. It supplies candidates, not canonical match authority.
- Graph owns vertices, labels, canonical properties, read-prefix execution, mutation seed rows, and
  the no-`await` canonical mutation segment.
- The generic `gleaph-gql` and `gleaph-gql-planner` crates continue to express ordinary scans,
  filters, joins, and Cartesian products. Gleaph-specific seed lowering remains in Router/Graph
  integration code.
- ADR 0030's constraint catalog and `GRAPH_LOCAL_UNIQUE_VALUES` already own declared uniqueness.
  They may provide a specialized access path; a second uniqueness map is neither required nor
  allowed.

The gap is a versioned relation transport plus seeded operator semantics that validate bound
candidates rather than deleting the corresponding plan predicates.

## Decision

### 1. Model the Router result as candidate domains, not authoritative match rows

Router extracts a `VariableAnchorSet` for every independently anchored variable in the supported
leading read prefix. Each set contains the variable and all usable label/equality restrictions for
that variable. Several restrictions on one variable are intersected before dispatch.

For each target shard, Router produces one candidate domain per variable:

```text
a -> [42, 43]
b -> [17, 18, 19]
```

The domain relation means "these subjects may satisfy this variable's anchor on this shard." It is
not a claim that every subject still matches canonical Graph state. A shard is omitted when any
required variable has an empty domain. Cross-shard combinations are not manufactured for a
shard-local mutation; existing federation admission and saga rules continue to decide whether a
different query shape may span shards.

Only independent leading anchors are admitted initially. A correlated predicate, dependency on a
previously computed value, optional binding, edge endpoint dependency, or unsupported join shape is
not guessed from syntax. It uses the existing exact fallback or is rejected by the existing
federation gate.

### 2. Resolve parameterized anchors per item while sharing work across the bulk group

Bulk grouping continues to plan and resolve catalogs once. Candidate resolution is item-specific:

1. decode every item's parameter map;
2. instantiate its variable anchor probes;
3. deduplicate identical lookup keys across the group by the full semantic key, including graph,
   label/index identity, property id, comparison kind, and encoded value;
4. execute distinct lookups through a bounded Property Index batch API or bounded parallel calls;
5. map the results back to `(item_index, shard_id, variable)` domains; and
6. attach that item's seed relation to its own `ExecutePlanArgs`.

The Router must not copy the first item's `seed_bindings_blob` to later parameter sets. An
empty-domain item produces a durable `NoDispatchZeroMatch` operation outcome. It may omit a Graph
call only when the Router's per-operation envelope preserves result position and the mapping to each
Graph canister's local operation cursor; absence of a dispatch must never shift later replay ordinals.

Lookup deduplication is invocation-local optimization only. Property Index and the immutable Router
mutation envelope remain the sources of replayable lookup output; no process-global candidate cache
is introduced.

### 3. Add a versioned seed-relation envelope

The current unversioned `SeedBindingsWire { entries, rows }` cannot represent candidate-domain
semantics without ambiguity. The opaque `ExecutePlanArgs.seed_bindings_blob` will therefore use a
dual decoder: existing bytes decode as the legacy raw struct, while new bytes decode as a versioned
inner envelope:

```text
DecodedSeedRelation::Legacy(LegacySeedBindingsWire)
SeedRelationEnvelope::V2(CandidateDomainSeedWire)

CandidateDomainSeedWire {
    domains: [
        CandidateDomainWire {
            variable,
            vertex_ids,
            required_vertex_label_ids,
        },
        ...
    ],
}
```

The implementation must retain the raw legacy decoder for already persisted seed blobs. `V2` domains
are ordered by the plan's binding layout, variable names are unique, local ids are sorted and
deduplicated, and every label id is Router-resolved. Empty or duplicate required domains, unknown
variables, mixed Graph/shard identities, and malformed ids are rejected before mutation. Graph
computes the product estimate with checked arithmetic; the wire does not duplicate that derived fact.

`SeedRowWire` remains the complete-row transport for leading `SEARCH` and other already-lowered
relations. It is not redefined as a general multi-variable Property Index result. This preserves one
meaning for each wire shape.

### 4. Generate products at the Graph execution boundary, bounded and preferably lazy

Router does not enumerate `A x B x ...`. Graph hydrates each candidate domain, then feeds the domains
to the existing read-prefix operators in binding-layout order. The preferred executor shape is a
lazy or chunked row producer so later filters and joins can reduce cardinality before every product
row is retained.

An initial implementation may materialize rows only after checked multiplication proves the result
fits the shared product-row and instruction bounds. Checked arithmetic is mandatory. Neither Router
nor Graph may truncate a relation and report partial success as the result of the original GQL
mutation.

The unavoidable semantic cost remains: if the query truly matches `|A| x |B|` rows and inserts one
edge per row, mutation work is `O(product)`. This ADR avoids paying that cost early in Router and on
the wire; it does not change GQL Cartesian-product semantics.

### 5. Revalidate the original anchor semantics against canonical Graph state

Graph must not implement V2 by stripping the complete leading prefix. Instead, seeded execution uses
bound-candidate semantics:

- bound `NodeScan(variable, label)` validates current local existence, tombstone state, and label;
- bound equality `IndexScan` reads the variable's current canonical property and compares it with
  the literal/parameter without calling Property Index;
- bound `IndexIntersection` validates every represented equality locally;
- residual `PropertyFilter`, joins, and Cartesian products execute normally; and
- edge candidates validate their current canonical handle, label/direction, property, and endpoints
  before they enter a mutation row.

The physical plan remains the single source of predicate and join semantics. The wire narrows the
subjects considered but does not duplicate or replace the domain rule. No remote lookup occurs
between this local validation and the canonical mutation segment.

A derived-index hit that became stale positive is therefore filtered out. Candidate completeness is
governed separately:

- a read mode whose index watermark satisfies the request may use the corresponding posting result
  as the routing snapshot;
- if the required projection watermark cannot be established, the path falls back to an exact
  Graph-local read or fails closed according to the caller's consistency contract; and
- callers that require matching against the latest canonical state at the Graph commit point need a
  Graph-local canonical access path or a full local scan. A pre-dispatch remote candidate set alone
  cannot prove absence of a newly matching vertex.

### 6. Persist the exact per-item relation for deterministic replay

ADR 0029 requires immutable seed bindings in the Router mutation envelope. The current
`RouterMutationShard` shape stores one seed blob per shard and cannot represent different relations
for several operations sharing one `MutationId`.

Bulk support for V2 therefore requires a new version of the Router mutation record containing an
ordered per-operation dispatch envelope. Each operation records its request/parameter fingerprint
and either `Dispatches(per_shard_seed_relations)` or `NoDispatchZeroMatch`. Seed relations may
reference an immutable deduplicated blob table in the same record. The envelope also records the
mapping from public item order to each Graph canister's shard-local operation ordinal. Recovery
resends the persisted relation, preserves zero-match positions, and never repeats Property Index
lookup to reconstruct a possibly different candidate set.

This is a stable-value schema evolution, not a new stable-memory region. V1 remains decodable.
Implementation must update the stable-memory inventory only if it introduces a new region rather
than versioning the existing value.

### 7. Apply shared, explicit bounds and fail closed

The implementation defines shared constants for:

- supported anchored-variable count;
- distinct bulk lookup count;
- candidates per variable and total candidates per item;
- checked estimated product rows;
- encoded seed relation and `ExecutePlanBatchArgs` byte size; and
- Graph instruction/row budget.

Exact values are selected with focused Router/Graph benchmarks and the existing safe inter-canister
payload ceiling. Exceeding a bound never silently truncates. The eligible alternatives are:

1. exact Graph-local scan/read-prefix execution;
2. the existing sequential mutation path when it preserves the requested consistency semantics; or
3. explicit broad-mutation rejection when neither bounded path is safe.

The fallback reason is observable in diagnostics. `batch-instr-log` may measure the decision but is
not part of correctness and remains disabled in ordinary deployment configuration.

### 8. Use declared constraints as optional access paths

Constraint acceleration is selected only from an `Active` Router constraint-catalog record. Data that
is merely observed or expected to be unique does not qualify.

- For `ShardLocalGlobal`, the existing canonical
  `(constraint_id, encoded_value) -> owner_element_id` table may gain a narrow owner lookup API.
  Graph can resolve an equality-bound variable and validate its label/property in the same
  no-`await` message segment. No duplicate map or new ownership domain is introduced.
- For `FederatedTcc`, a confirmed reservation may narrow Router routing to at most one owner, but
  Graph still revalidates canonical element state before mutation.
- Label, type, `NOT NULL`, and future constraints may reduce candidate or validation work only when
  their catalog status and enforcement strategy prove the optimization sound.

The planner remains generic. Router/Graph integration selects the constraint-backed access path from
resolved catalog metadata; no Gleaph-only constraint operator is added to `gleaph-gql-planner`.

The general candidate-domain implementation is the baseline. A constraint fast path must have the
same observable rows, errors, idempotency, and mutation effects as that baseline.

## Alternatives considered

### Let Graph full-scan every item

This preserves canonical ownership and requires no wire change, but repeats work despite selective
indexes and defeats ADR 0044's throughput goal. Retained only as an exact fallback.

### Materialize all `SeedRowWire` products in Router

This reuses an existing type and is acceptable for a proved-small 1x1 relation, but has
`O(product)` Router memory and wire size. It also encourages treating remote hits as authoritative
rows. Rejected as the general representation.

### Send complete rows and skip the entire read prefix

Fast, but stale index hits can bypass canonical property validation and remote inputs become the
mutation authority. Rejected because it conflicts with ADR 0029 and duplicates plan semantics in
the wire.

### Reinterpret multiple legacy `entries` as a Cartesian product

This avoids a new type but changes an existing unversioned meaning and cannot distinguish union,
intersection, independent domains, and complete rows safely. Rejected.

### Depend only on UNIQUE lookup

Efficient for declared unique keys but does not cover non-unique indexed predicates and would tempt
the implementation to infer constraints from data. Rejected as the baseline; retained as an
optional specialization.

### Add another canonical index canister

A remote canonical index would add a second canonical write domain and cannot join Graph's local
atomic segment. Rejected. Canonical graph properties remain Graph-owned; Property Index remains a
derived projection.

## Consequences

- Multi-variable parameterized mutations can remain on the bulk path without sharing the first
  item's seeds or forcing one full scan per item.
- Router lookup work is proportional to distinct anchor values, and wire size is proportional to
  candidate-domain size rather than Cartesian-product size.
- Graph remains the canonical predicate and mutation authority.
- Replay becomes deterministic because the exact per-item relation is durable.
- The new inner wire envelope and Router mutation-record version require compatibility tests and a
  stable-value migration audit.
- Broad non-unique anchors still have unavoidable semantic cost and may fall back or reject.
- The strongest latest-canonical completeness semantics still require a Graph-local access path or
  scan; a remote derived index cannot provide that guarantee by itself.
- Declared constraints improve performance without creating a second definition of uniqueness.

## Implementation and validation status

This ADR remains **Proposed** for the full candidate-domain/V2 design. **Phase 1** of the multi-
variable bulk path is implemented as of 2026-07-21 UTC, and a **Phase 1 extension** for selective
single-variable anchored mutations was implemented the same day.

Phase 1 implementation (multi-variable):

- `SeedAnchorSet` parses multiple independently anchored variables from the leading read prefix.
- Multi-variable prefixes require every anchored variable to have at least one non-label equality
  anchor; label-only multi-variable prefixes still fall back to Graph-local execution.
- The bulk path detects multi-variable seeds and resolves per-item per-shard candidate domains,
  materializing a bounded Cartesian product (≤1024 rows) into complete `SeedRowWire` rows.
- `SeedBindingsWire.complete_prefix_rows: bool` signals that the rows are complete for the entire
  read prefix. When true, Graph skips the read phase entirely and feeds the seed rows directly to
  the canonical mutation segment.
- Empty domains produce a durable zero-row complete-prefix relation, so the item reports zero
  matches without requiring a separate Router short-circuit.

Phase 1 extension (single-variable):

- The same complete-row mechanism applies to a single anchored variable whose seed can be resolved
  from at least one selective non-label equality/index anchor (`IndexScan` equality or
  `IndexIntersection`).
- Label-only, edge, correlated, optional, and uniqueness-constrained single-variable mutations
  remain fail-closed on the existing scalar fallback.
- Per-item parameter binding resolves independently for every bulk item and target shard; the first
  item's seed is never reused for later items.
- Zero-hit items retain their response position through an empty complete-prefix row set, with no
  per-item journal entry.

Phase 1 deliberately does **not** implement:

- the candidate-domain V2 envelope;
- lazy or chunked product generation;
- cross-shard routing for multi-variable products;
- bulk lookup deduplication across items;
- declared-constraint fast paths; or
- deterministic per-item relation persistence beyond the existing seed blob.

Focused tests added:

- multi-variable `SeedAnchorSet` extraction for the wave 4 `demo_id` shape;
- bounded Cartesian-product generation and its row limit;
- Graph execution with complete row seeds for the wave 4 plan shape; and
- Graph canonical revalidation filtering out stale property values and removed labels.

Remaining work for the full ADR includes candidate-domain V2 wire, Graph bound-anchor
revalidation, lazy/chunked products, cross-shard routing, bulk lookup deduplication, payload and
instruction bounds shared across Router and Graph, V1/V2 decode compatibility, deterministic retry,
constraint-catalog gating, and focused canbench coverage comparing full scan, 1x1 candidate domains,
repeated-value bulk deduplication, bounded non-unique products, and `ShardLocalGlobal` owner lookup.

ADR 0047 now owns the shared typed bulk execution envelope that will carry per-operation seeds
(including candidate-domain and complete-row seeds) from Router to Graph and persist them for
deterministic replay. ADR 0046 remains the owner of candidate-domain semantics, Graph canonical
revalidation, and bound-anchor executor behavior.

## Related documents

- [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md): canonical Graph
  atomicity and immutable Router mutation envelopes.
- [ADR 0030](0030-cross-shard-uniqueness-tcc-reservation.md): declared constraint ownership and
  `ShardLocalGlobal` canonical unique-value table.
- [ADR 0044](0044-router-bulk-mutation-key.md): bulk mutation identity, progress, and per-item
  result mapping.
- [ADR 0047](0047-shared-typed-graph-bulk-envelope.md): shared typed bulk execution envelope
  for per-operation seed replay.
- [Physical plan format](../gql/plan-format.md): seed transport and executor assumptions.
- [Execution pipeline](../execution/pipeline.md): Graph seed hydration and read-prefix execution.
