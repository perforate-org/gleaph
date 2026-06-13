# 0012. Edge index direction in CREATE INDEX (GQL-aligned FOR patterns)

Date: 2026-06-13  
Status: proposed  
Last revised: 2026-06-13  
Anchor timestamp: 2026-06-13 10:15:54 UTC +0000

## Revision history

| Date | Change |
|------|--------|
| 2026-06-13 | Proposed; GQL `EdgeDirection` in edge `CREATE INDEX FOR`; wire label in graph-index keys; planner subset rule. |

## Context

[ADR 0009](0009-edge-property-index-and-index-ddl.md) Phase E implemented opt-in `CREATE INDEX` /
`DROP INDEX` for edge properties. The edge DDL example uses:

```gql
FOR ()-[e:KNOWS]-() ON (e.weight);
```

Phase E implementation and PocketIC e2e cover **directed** leading `EdgeIndexScan` queries such as
`MATCH ()-[e:KNOWS {weight: 5}]->(b)`. Three gaps remain:

| Gap | Detail |
|-----|--------|
| **Direction omitted in registry** | `IndexDefRecord` stores `(kind, label_id, property_id)` only. Directed and undirected edges with the same catalog label collapse to one index entry. |
| **graph-index key uses ambiguous `label_id`** | ADR 0009 §1 defines `label_id` as `EdgeLabelId` catalog raw. Graph DML enqueues postings using **LARA wire** keys (`BucketLabelKey`: directed MSB + label index). Router `lookup_edge_equal(..., Some(catalog))` can miss postings or bind the wrong CSR bucket. |
| **Planner stats are direction-blind** | `GraphStats::is_edge_property_indexed(property)` ignores query `EdgeDirection`. An index registered for `->` must not satisfy `~ ~` queries and vice versa. |

Gleaph GQL ([`EdgeDirection`](../../crates/gql/src/types.rs)) defines **seven** edge directions
(full bracket and simplified slash forms normalize to the same enum). Administrators expect
`CREATE INDEX` `FOR` patterns to use the **same surface** as `MATCH` / `INSERT` edge patterns.

### Prerequisites (met)

- ADR 0009 Phases A–E (edge postings on graph-index, router seeds, extension DDL)
- GQL pattern parser ([`pattern.rs`](../../crates/gql/src/parser/pattern.rs)) — direction resolution from bracket / slash tokens
- LARA labeled CSR wire keys ([`BucketLabelKey`](../../crates/ic-stable-lara/src/labeled/bucket_label_key.rs); ADR [0008](0008-edge-payload-profile-router-ssot.md) catalog SSOT)

### Non-goals (this ADR)

- Label expressions in DDL (`|`, `&`, `!`, quantifiers on the indexed edge)
- Edge `WHERE` clauses in `CREATE INDEX FOR`
- Multi-hop paths in `FOR` (more than one edge element)
- Range / vector edge indexes (`USING …`)
- Automatic migration of existing Phase E indexes without admin `DROP` + `CREATE` (dev discard OK per ADR 0009)

---

## Decision

### 1. `FOR` clause parses Gleaph GQL edge patterns (v1: single edge)

Edge `CREATE INDEX` syntax:

```gql
CREATE INDEX <index_name> [IF NOT EXISTS]
  FOR <left_endpoint> <edge_pattern> <right_endpoint>
  ON (<edge_var>.<property>);
```

**v1 constraints**

| Constraint | Rule |
|------------|------|
| Endpoints | `()` or `(var:Label)`; v1 MAY require `()` on both sides (no endpoint labels in DDL) |
| Edge count | Exactly one edge element |
| Edge pattern | Full bracket **or** simplified slash form accepted by `gleaph-gql` |
| Label | Single catalog name (`:KNOWS`); no label expressions |
| `ON` | Property access on the edge variable declared in `FOR` |

**Examples** (all seven `EdgeDirection` values):

```gql
-- PointingRight          -[e:L]->     or  -/L/->
CREATE INDEX w_right  FOR () -[e:KNOWS]-> ()  ON (e.weight);

-- PointingLeft           <-[e:L]-     or  -/<L/-
CREATE INDEX w_left   FOR () <-[e:KNOWS]- ()  ON (e.weight);

-- LeftOrRight            <-[e:L]->    or  -/<L/->
CREATE INDEX w_lr     FOR () <-[e:KNOWS]-> () ON (e.weight);

-- Undirected             ~[e:L]~      or  ~/L/~
CREATE INDEX w_undir  FOR () ~[e:KNOWS]~ ()   ON (e.weight);

-- UndirectedOrRight      ~[e:L]~>     or  ~/L/~>
CREATE INDEX w_uor    FOR () ~[e:KNOWS]~> ()  ON (e.weight);

-- LeftOrUndirected       <~[e:L]~     or  <~/L/~
CREATE INDEX w_lou    FOR () <~[e:KNOWS]~ ()  ON (e.weight);

-- AnyDirection           -[e:L]-      or  -/L/-
CREATE INDEX w_any    FOR () -[e:KNOWS]- ()   ON (e.weight);
```

**Parsing:** replace the ad-hoc edge branch in [`index_ddl.rs`](../../crates/router/src/index_ddl.rs)
with a thin wrapper around `gleaph-gql` pattern parsing that produces
`(EdgeDirection, edge_variable, label_name)`. Vertex `FOR (n:Person)` parsing stays as today.

### 2. Index registry stores `EdgeDirection`

Extend `IndexDefRecord` (and human-facing `IndexCatalogEntry`) for edge indexes:

```rust
struct IndexEdgeTarget {
    label_id: EdgeLabelId,       // catalog id
    property_id: PropertyId,
    direction: EdgeDirection,    // gleaph_gql::types::EdgeDirection
}
```

| Field | Role |
|-------|------|
| `index_name` | Admin-chosen name; unique per logical graph (ADR [0011](0011-gql-graph-resolution-and-catalog-scoping.md)) |
| `(label_id, property_id, direction)` | Semantic identity of the index **in addition to** `index_name` |
| Uniqueness | At most one named index per `(graph_id, label_id, property_id, direction)` |

**Multiple indexes** on the same `(label, property)` with different directions are allowed (e.g.
`w_right` and `w_undir` on `(KNOWS, weight)`).

`ROUTER_INDEXED_PROPERTY_SET` membership for shard fan-out remains **property-level** (any edge
index on `weight` registers `weight` on shards). Shard DML gates posting enqueue using the full
`(label_id, property_id, direction)` entries loaded from the named-index catalog.

### 3. Storage classes: query direction vs CSR wire

CSR stores edges under **LARA wire** labels (`BucketLabelKey`), not bare catalog ids:

| Storage class | Wire shape | Example (catalog `KNOWS` = 1) |
|---------------|------------|-------------------------------|
| Directed | MSB set | `0x8001` |
| Undirected | MSB clear, non-zero index | `0x0001` |

Define **storage classes maintained** by an index from its registered `EdgeDirection`:

| Registered `EdgeDirection` | Maintain postings for |
|----------------------------|------------------------|
| `PointingRight`, `PointingLeft`, `LeftOrRight` | Directed only |
| `Undirected` | Undirected only |
| `AnyDirection`, `LeftOrUndirected`, `UndirectedOrRight` | Directed **and** undirected |

**DML:** when `set_edge_property` / delete runs on a canonical edge handle, enqueue graph-index
postings only for registered indexes whose maintained storage classes include the edge’s wire
class.

**Planner index applicability:** a query with direction `Q` may use an index registered with
direction `I` iff:

```text
storage_classes(Q) ⊆ storage_classes(I)
```

Examples:

| Index `I` | Query `Q` | Usable? |
|-----------|-----------|---------|
| `AnyDirection` | `PointingRight` | Yes |
| `PointingRight` | `AnyDirection` | No (undirected edges missed) |
| `LeftOrRight` | `PointingLeft` | Yes |
| `Undirected` | `PointingRight` | No |

Implement `GraphStats::is_edge_property_indexed(label, property, direction)` (or equivalent
planner hook) with this subset rule.

### 4. graph-index edge key: use wire label, not catalog-only

**Amends ADR 0009 §1** edge posting key field `label_id`:

```text
(property_id, value, wire_label_id, shard_id, owner_vertex_id, slot_index)
```

| Field | Role |
|-------|------|
| `wire_label_id` | `BucketLabelKey` raw `u16` on the owning shard (directed / undirected bit included) |
| Other fields | Unchanged from ADR 0009 |

**DML → index:** convert canonical edge handle wire label to posting key **without** stripping the
directed MSB.

**Router lookup:** given query direction `Q` and catalog label `L`, compute the set of wire prefixes:

```text
W(Q, L) = { pack(L, storage_class).raw() | storage_class ∈ storage_classes(Q) }
```

`lookup_edge_equal(property_id, value, label_filter)` accepts either:

- `LabelFilter::Wire(u16)` — exact wire prefix (single-class queries), or
- `LabelFilter::Catalog { id, classes }` — expand to one or two wire prefix scans (implementation choice)

**Seed / bind:** `LocalEdgePosting.label_id` and `EdgePostingHit.label_id` carry **wire** labels.
Graph shard seed apply uses the wire value directly in `EdgeHandle` (no catalog→directed guess).

Unlabeled edges remain `wire_label_id = UNLABELED_DIRECTED (0x8000)` or `UNLABELED_UNDIRECTED (0)`
per ADR 0009 §1.1; edge DDL still requires a catalog label in `FOR`.

### 5. Planner, router seeds, and executor

| Component | Contract |
|-----------|----------|
| **Planner** | Emit `EdgeIndexScan` / `indexed_edge_equality` only when an applicable index exists for `(label, property, query_direction)` |
| **Router `IndexAnchor::EdgeEqual`** | Carry catalog label id **and** query `EdgeDirection` (or precomputed wire filter) |
| **graph-index lookup** | Filter postings by wire label set `W(Q, L)` |
| **Graph executor** | Seed bind + local edge-index scan unchanged except wire-native labels |
| **Expand scan fallback** | After `DROP INDEX`, inline edge property filters still resolve properties via `property_uses` (ADR 0009 Phase E) |

Leading `EdgeIndexScan` eligibility ([`first_hop_supports_leading_edge_index`](../../crates/gql-planner/src/planner/match_plan/path/pattern/lower.rs)) remains limited to
`PointingRight` and `PointingLeft` hops; other directions use expand-path index fusion or scan
fallback per existing planner rules.

### 6. `DROP INDEX` and backfill

- `DROP INDEX` removes the `(label, property, direction)` registry entry for that name.
- When no remaining index references `(kind=Edge, property_id)`, fan out `unregister_indexed_property` to shards (ADR 0009).
- Posting purge: range delete by `(property_id, …)` on graph-index OR replay backfill after purge (same eventual-consistency choice as ADR 0009 §4).
- `admin_edge_property_backfill_step` replays from `EDGE_PROPERTIES` using wire labels and registered direction entries.

---

## Implementation phases

| Phase | Deliverable | Verification |
|-------|-------------|--------------|
| **F1 — ADR + key layout** | graph-index `wire_label_id` key; stable layout bump / dev discard | graph-index unit tests; key ordering tests |
| **F2 — Registry + DDL** | GQL-aligned `FOR` parser; `IndexDefRecord.direction`; uniqueness | router `index_ddl` tests for 7 directions + slash forms |
| **F3 — DML postings** | Direction-aware enqueue; remove catalog-only conversion | graph unit tests; posting records wire labels |
| **F4 — Lookup + seeds** | Wire-aware `lookup_edge_equal`; router anchor carries direction | router seed tests; graph seed apply |
| **F5 — Planner stats** | Subset rule in `GraphStats`; direction-aware edge index fusion | gql-planner tests |
| **F6 — PocketIC e2e** | `->`, `~`, `-` indexes; DROP; federated anchor loss | `router_gql_query` edge direction cases |

Phase E (ADR 0009) indexes without `direction` are **invalid** after F2 in dev environments; operators
recreate indexes with explicit `FOR` patterns.

---

## Consequences

### Positive

- Administrators use the same edge direction syntax in DDL as in GQL queries.
- Directed and undirected storage no longer collide in graph-index.
- Seed bind is wire-accurate; removes catalog-only posting / directed-guess bugs.
- Subset rule makes “broad index serves narrow query” predictable.

### Negative / costs

- Stable layout change on graph-index edge postings (`wire_label_id` semantics).
- Up to seven registry rows per `(label, property)` if an operator indexes all directions.
- Planner and stats API surface grows (direction parameter).
- Phase E DDL example `()-[e:KNOWS]-()` must be documented as `AnyDirection`, not “directionless default”.

### Risks

| Risk | Mitigation |
|------|------------|
| Operators confuse `LeftOrRight` with “directed only” | Document mapping table (§1 examples); `SHOW INDEXES` follow-up |
| Multiple wire scans for `AnyDirection` lookup | Two prefix probes (directed + undirected); acceptable at standalone scale |
| Slash vs bracket typo in DDL | Parser errors cite GQL token expectation |

---

## Alternatives considered

| Alternative | Why not |
|-------------|---------|
| Keep catalog-only `label_id` in index keys | Cannot distinguish directed / undirected; bind requires heuristics |
| Explicit `WITH EDGE DIRECTEDNESS` clause instead of pattern | Duplicates GQL; two ways to express direction |
| Exact direction match only (no subset rule) | Requires redundant indexes; `AnyDirection` index useless for narrow queries |
| Store `EdgeDirection` in posting key | Seven-way key explosion; storage class (2 values) is enough for DML |

---

## References

- [ADR 0009](0009-edge-property-index-and-index-ddl.md) — edge postings, extension DDL (amended §1 key field)
- [ADR 0008](0008-edge-payload-profile-router-ssot.md) — catalog SSOT (orthogonal to index wire keys)
- [property-index.md](../index/property-index.md) — derived index semantics
- GQL §16 edge directions — [`crates/gql/tests/section_tests/s16/`](../../crates/gql/tests/section_tests/s16/)
