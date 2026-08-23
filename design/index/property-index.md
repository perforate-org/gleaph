# Property index

Last updated: 2026-08-22
Anchor timestamp: 2026-08-22 13:29:10 UTC +0000

## Status

**Partially Implemented** — `lookup_equal` / `lookup_range` and DML posting sync exist. **`lookup_intersection`** is implemented on graph-index; router `IndexAnchor` + per-shard seeds and graph skip of leading intersection op are **Implemented**. Graph federated wire path does not call index; library tests may still inject a mock index client.

The shard-to-index update boundary also exposes a bounded `posting_batch`
transport. It processes the largest safe prefix under the index canister's own
instruction budget and returns continuation progress; the individual posting
operations remain the index's owning mutation primitives.

Equality candidate paging also exposes `lookup_equal_page_for_label`, which
applies vertex-label membership inside the Property Index before returning the
page. Range candidate paging likewise exposes `lookup_range_page_for_label` with
the same index-owned label sieve and encoded interval cursor semantics. These
compound reads avoid a Router-to-index round trip for equality and range filter
pages.

The range storage primitive is implemented with ordered posting scans
(`StableBTreeMap::range()`), Router `SEARCH ... WHERE` uses the paginated
vertex range endpoints, and normal `MATCH` planning selects range anchors
through the same Active vertex catalog projection as equality, with the Router
seeding graph shards via `IndexAnchor::Range` (GAP-2026-07-29-002, closed
2026-08-21). Edge postings expose the same ordered contract through the
paginated `lookup_edge_range_page` endpoint and `IndexAnchor::EdgeRange`
(GAP-2026-07-29-003, closed 2026-08-22).

**Edge ranges — Implemented (drain+clamp decision).** Edge property range
predicates are served from ordered edge postings on both execution paths:

- graph-index scans `(physical_index_id, property_id, value, ...)` value
  intervals of `INDEX_EDGE_POSTINGS` in full edge posting key order via
  `lookup_edge_range_page` (`PostingRangeRequest` bounds, optional wire-label
  sieve applied inside the index call, cursor-clamped resumable pages). The
  half-open key bounds live in one owner next to the vertex bounds
  (`crates/graph-index/src/posting_range.rs`).
- The Router projects edge range capability from the same Active edge catalog
  membership as equality (`is_edge_property_range_indexed_for`, fail-closed for
  unindexed properties), lowers a leading one-sided indexed edge predicate to
  `PlanOp::EdgeIndexScan { cmp }` (equality wins first; two-sided bounds ride
  residual conjunction like GAP-002), seeds shards through
  `IndexAnchor::EdgeRange`, and collects hits **drain+clamp**: every page is
  drained within the comparison-domain-clamped interval, then rows bind.
  IC query execution is one-shot per call, so no cursor state survives the
  call; residual predicates stay as filters exactly like the vertex
  two-sided-bounds note in GAP-002. Wire label sieving (ADR 0012 direction
  subset rule) is preserved by fanning one drained lookup out per wire label
  with `(shard_id, owner_vertex_id, label_id, slot_index)` deduplication.
- The single-shard executor derives the same clamped `Between` interval through
  the shared helper for supported literal domains; an empty clamped interval is
  answered as zero candidates, and unsupported domains (Bool/List/Record/Path/
  Extension/Duration) fall back to the canonical `EDGE_PROPERTIES` filtered
  superset scan with the plan's residual filter deciding exact matches.

See [implementation-gaps.md](../implementation-gaps.md)
GAP-2026-07-29-003 for evidence links.

**Edge posting label identity (canonical owner: `gleaph_graph_kernel::entry::label`).**
Stored edge postings always carry the LARA wire tag — `catalog id | directed MSB` for
directed buckets, the bare catalog id for undirected buckets — while registrations,
catalog memberships, and lookup requests speak catalog ids. The two spaces meet only
through `EdgeLabelId::pack` / `TaggedEdgeLabelId::label_index`: graph-index accepts a
seed fact or DML build subject when its wire label's catalog index equals the registered
catalog label and its bucket is covered by the registration direction (storing the posting
under the wire tag, so read-side `EdgeHandle` binding resolves the CSR bucket directly),
and labeled lookups fan out over both packings of the requested catalog id. Router
registration targets, export scopes/walks, and all Graph producers stay in their native
space; no ad-hoc `0x8000` arithmetic exists outside the kernel owner
(GAP-2026-08-22-001).

**Comparison-domain bounds (canonical owner: `gleaph_gql::value_index_key`).**
Every consumer derives `property OP <literal>` scan intervals through one
helper — `range_bounds(value, op)` for literals and parameters, and
`range_bounds_for_encoded_key(bytes, op)` for cached probe bounds — never by
reconstructing tag prefixes or ceilings itself. The helper pins each literal to
its posting-key comparison domain and clamps every upper bound with
`min(succ(bound), ceiling)` so an all-0xFF tail (e.g. `DateTime(i64::MAX,
u32::MAX)`) cannot spill into the next tag or subtype domain:

| Literal domain | Posting-key interval `[floor, ceiling)` |
| --- | --- |
| NUMERIC (all widths, Decimal, Float) | `[2] .. [3]` |
| TEXT | `[6] .. [7]` |
| BYTES | `[7] .. [8]` |
| TEMPORAL Date / Time / LocalTime / DateTime / LocalDateTime / ZonedDateTime / ZonedTime | `[8, k] .. [8, k+1]` for subtype `k` in 1..=7 |

Bool, List, Record, Path, Extension, Null, and Duration (tuple order is not
chronological) have no contiguous ordered interval and reject with
`ValueIndexKeyError::UnsupportedRangeDomain`. Pushdown eligibility by literal
domain has exactly one gate, consulted where the resolved value lives:
graph's single-shard executor falls unsupported domains back to the non-index
filter path (node scan plus residual filter), the Router skips seeding for them
(they cannot form a range anchor), and `SEARCH ... WHERE` rejects them with an
explicit error. Because every request is clamped to the literal's own domain, a
one-sided range over a mixed-type property can never return rows whose stored
value lies outside the literal's comparison domain. An empty clamped interval
(`low >= high`, e.g. strictly greater than the domain maximum) is answered as
zero hits without calling the Property Index, whose structural validation
rejects such bounds.

The compound intersection reads `lookup_intersection_page_for_label` and
`lookup_range_intersection_page_for_label` apply the same label sieve after the
index-local equality/range intersection and preserve the walk-arm cursor. The
Router uses these endpoints for label-filtered `SEARCH` intersection paths, so
each page requires one Router-to-index query instead of a separate label-filter
call.

**ADR 0034 Slice 14 — Implemented:** `lookup_range_intersection_page` supports one to eight equality arms combined with a single finite comparison-domain range on a distinct vertex property. The index walks the finite encoded range one page at a time, sieves each page server-side against every equality arm, and preserves the range cursor even when a survivor page is empty. Zero equality arms and more than eight arms are rejected; non-vertex specs are rejected.

**ADR 0034 Slice 15 — Implemented (Router-side):** same-property equality disjunctions for `SEARCH ... WHERE` are executed by the Router as a union of up to eight `lookup_equal_page` streams against the same `(property_id, label_id, graph_id)` active vertex property index. The Property Index endpoint contract does not change; `lookup_equal_page` remains the primitive for a single `(property_id, value)` bucket. The Router collects per-arm per-source pages, applies label-scoped filtering, globally deduplicates `(shard_id, vertex_id)` subjects, and enforces the existing 4096 candidate bound before Vector Index ranking.

**ADR 0034 Slice 16 — Implemented (Router-side):** cross-property pure equality disjunctions for `SEARCH ... WHERE` are executed by the Router as a union of up to eight `lookup_equal_page` streams, one stream per distinct `(property_id, encoded_value)` source. Each arm resolves its own `(property_id, label_id, graph_id)` active vertex property index; the Property Index endpoint contract still does not change. The Router applies the same per-page label filtering, global `(shard_id, vertex_id)` deduplication, and 4096 candidate bound as Slice 15.
**ADR 0034 Slice 17 — Implemented (Router-side):** same-property comparison-domain range disjunctions for `SEARCH ... WHERE` are executed by the Router as a union of up to eight finite encoded domain intervals against the same `(property_id, label_id, graph_id)` active vertex property index. Each arm is normalized to a half-open encoded interval; empty or contradictory intervals are dropped; the remaining intervals are sorted and merged into disjoint encoded bounds before any lookup. The candidate set is collected through paginated `lookup_range_page_for_label` calls with `PostingRangeRequest::Between`; the index canister applies label membership before returning each page, followed by global `(shard_id, vertex_id)` deduplication and the 4096 candidate bound. The Property Index endpoint exposes the label-sieved range primitive so label filtering does not require a second inter-canister call.

**ADR 0034 Slice 18 — Implemented (Router-side):** cross-property comparison-domain range disjunctions for `SEARCH ... WHERE` are executed by the Router as a union of up to eight finite encoded domain intervals across one or more `(property_id, label_id, graph_id)` active vertex property indexes. Each arm resolves its own property id and is normalized to a half-open encoded interval; empty or contradictory intervals are dropped; intervals are grouped by property id and merged **within each group** into disjoint encoded bounds. The candidate set is collected through paginated `lookup_range_page_for_label` calls with `PostingRangeRequest::Between` for every merged interval across all involved properties; the index canister applies label membership inside each call, followed by global `(shard_id, vertex_id)` deduplication and the 4096 candidate bound. Intervals are never merged across property ids because encoded keys are property-specific.

**Phase A ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md)) — superseded by [ADR 0023](../adr/0023-federated-index-consistency-upgrade-compaction.md):** Phase A gated DML/backfill posting maintenance with a **persistent shard-local registry** (`register_indexed_property`) fanned out from the router. ADR 0023 (phases 1–2 implemented) **removes that registry** because it could not survive the upgrade boundary. The graph shard now holds **no persisted indexed catalog**: the router (definitions SSOT) supplies an `IndexedPropertyCatalog` in `ExecutePlanArgs.indexed_properties` (and in the backfill request), which the shard installs in an **ephemeral per-operation context** (`index/catalog_context.rs`) consulted by `dispatch_property_index_ops` and backfill. `CREATE INDEX` / `DROP INDEX` no longer fan registrations out to shards.

**Phase B (ADR 0009) — Implemented:** `INDEX_EDGE_POSTINGS` on graph-index (`EdgePostingKey`); `edge_posting_insert` / `remove`, `lookup_edge_equal`; federated graph `edge_pending` flush; `backfill_edge_property_postings`.

**Phase C (ADR 0009) — Implemented:** `IndexSubject` on `IndexEqualSpec`; `lookup_intersection` returns `IndexIntersectionResult` (`Vertices` or `Edges`); mixed vertex+edge arms project to `(shard_id, owner_vertex_id)`.

**Phase D (ADR 0009) — Implemented:** router `EdgeIndexScan` / all-edge intersection → `lookup_edge_equal` / `lookup_intersection`; per-shard `LocalEdgePosting` seeds; graph applies edge seeds and skips leading `EdgeIndexScan`; shard-local `EDGE_EQUALITY_POSTINGS` retired (MemoryId repack to 40 regions); expand reads graph-index or canonical `EDGE_PROPERTIES` scan when no index client.

**Phase E (ADR 0009) — Implemented:** router extension DDL `CREATE INDEX` / `DROP INDEX` (parsed by the shared Gleaph vendor DDL owner `gleaph-index-ddl`, executed on `gql_execute*`); named index catalog per logical graph; Admin or Manager+ auth. Legacy `admin_set_indexed_*` APIs delegate to `CREATE INDEX IF NOT EXISTS` with synthetic index names (require entity label + property).

**ADR 0059 — Partially implemented:** migration-driven `CREATE INDEX`
selector/checksum, Graph export, graph-index pull, `PhysicalIndexId` namespace, touched-first
outbox, seal, Active-only planner lifecycle, and the production Router cross-canister driver are
implemented there. Graph label gain/loss transitions now use the exact `(label_id, property_id)`
membership supplied by the ephemeral Router catalog: the Graph coordinator admits all affected
Building namespaces before canonical label mutation, dispatches Active namespaces through the
ordinary posting queue, and rejects Sealing before mutation. The focused Graph regressions cover
the gain/loss, public mutation-id-0 wrapper, and delete single-emission boundaries. Focused PocketIC E2E and upgrade validation
remain pending; ordinary catalog registration and operator backfill therefore remain separate. ADR
0059 is the normative source for that protocol.

**ADR 0012 — Implemented:** edge `FOR` patterns carry Gleaph GQL `EdgeDirection` (bracket form only; slash rejected); graph-index edge keys use LARA `wire_label_id`; planner applies storage-class subset rule via `is_edge_property_indexed_for`. Leading `EdgeIndexScan` supports `PointingRight`, `PointingLeft`, and `Undirected`. PocketIC e2e covers `AnyDirection`, `PointingRight`, and `Undirected` indexes (including federated undirected anchor and subset negative: undirected-only index does not seed directed wire postings). See [0012-edge-index-direction-in-ddl.md](../adr/0012-edge-index-direction-in-ddl.md).

**ADR 0018 — Implemented:** property and label **name → id** catalogs are **graph-scoped** per `GraphId` on the router. Plan build, DDL, and `admin_intern_*` resolve names in the effective graph context. Posting keys are unchanged — numeric `property_id` on the wire is already scoped by dispatch `GraphId`.

**ADR 0019 — Partially Implemented:** per-graph index clusters and graph-local `ShardId`; router `graph_index_lookup_targets` derives from **live shard registry**; shard unregister detaches auth and purges postings for the detached shard in **bounded, resumable steps**. Graph Index owns a durable, checked-monotonic detach generation in its existing ownership cell, rejects reattach while that session is active, and accepts a returned `ShardDetachCursor` only when its generation exactly matches the API shard before any posting-key decode/read/write. The router resumes until `done` but owns no lifecycle state. Legacy and completed-session cursors fail closed, so detach/reattach/detach cannot replay D1 into D2. See [0019-graph-local-shard-id-and-index-clusters.md](../adr/0019-graph-local-shard-id-and-index-clusters.md).

Property **name → `property_id`** assignment is **router SSOT per `GraphId`** ([ADR 0006](../adr/0006-pre-federation-foundation.md) §2, scope amended by [ADR 0018](../adr/0018-graph-scoped-label-property-catalogs.md)). Graph shards no longer maintain `PROPERTY_CATALOG` stable; DML and plan execution use router-resolved `PropertyId` on the wire (`ResolvedPropertyTable`).

## Purpose

Explain the **graph-index canister** and how the router uses it for query routing (standalone: sole shard `ShardId(0)`; per-shard slice in the federation target).

## Non-goals

- Index build algorithms on graph writes (implementation in `graph/src/index/`).
- Migration-driven online `CREATE INDEX` backfill and activation are specified by [ADR 0059](../adr/0059-create-index-migration-backfill.md) and are **implemented** with cross-canister PocketIC proof (convergence, label fence, same-wasm upgrade reopen; GAP-2026-07-29-006 closed 2026-08-22); edge `INLINE` enumeration remains open under GAP-2026-07-29-001.
- Full Candid API listing.
- Index canister sharding (multiple index canisters) — **Partially Implemented** per-graph `index_cluster` and shard-group formula ([ADR 0019](../adr/0019-graph-local-shard-id-and-index-clusters.md)); subject/range split axes remain planned ([ADR 0010](../adr/0010-index-sharding-extensibility.md), [capacity-planning.md](capacity-planning.md)).

## Components

| Piece             | Crate                    | Role                                  |
| ----------------- | ------------------------ | ------------------------------------- |
| Posting key/value | `graph-index`            | Property equality postings            |
| `PostingHit`      | `graph-kernel`           | `{ shard_id, vertex_id }`             |
| Router client     | `router/index_client.rs` | `lookup_equal`, `lookup_intersection` |
| Seed resolution   | `router/seed.rs`         | Map hits → per-shard seed blobs       |

## Catalog ownership

| Layer           | Owns                                                                                                                                      |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| **Router**      | Property names ↔ `PropertyId` **per `GraphId`** (`ROUTER_PROPERTY_CATALOG`); planner / DML resolve names in graph context before dispatch |
| **Graph shard** | `(property_id, Value)` on vertices and edges only — no property name stable                                                               |
| **Graph-index** | Postings keyed by router-issued `property_id` (interpreted under the owning graph's dispatch / index-cluster boundary)                    |

Standalone and test graphs without router resolution use hash-based test property ids (`crates/graph/src/test_labels.rs`) or explicit `ResolvedPropertyTable` on the plan wire.

## On-demand provisioning (ADR 0035 Slice 10)

**Implemented (Router-side).** In provisioned mode (a `provision_canister` is configured), a
`CREATE INDEX` on a graph whose live shard groups have no index canister provisions one index
canister per unassigned group through the shared admission flow
(`requested_resources = [PropertyIndex(IndexClusterId(g)) for g in unassigned_groups]`), assigns
each to `index_cluster`, and retrofit-attaches every live shard in the affected groups to its
group's canister. Dev mode (no `provision_canister`) registers the definition indexless as before.
The `CREATE INDEX` path is async because provisioning is a cross-canister call.

Unlike vector (which separates provision from a manual `attach_vector_shard` admin API), property
index attach runs inside `CREATE INDEX`: there is no existing retrofit attach path for property
index, and the index canister is unusable until its shards are attached. `unassigned_index_groups`
keys off the shards' own `index_canister` (not `index_cluster`) so a group whose canister was
assigned but whose shards were not yet attached is re-provisioned idempotently on retry;
`attach_provisioned_index_canisters` is idempotent per group.

## Posting model

Postings keyed by `(property_id, encoded_value, shard_id, local_vertex_id)` — **no `GraphId` in the key** ([ADR 0010](../adr/0010-index-sharding-extensibility.md)).

- `property_id` — numeric id from the router catalog **for the dispatch `GraphId`** (same id the graph shard uses when writing values for that graph).
- `shard_id` — graph-local shard ordinal within the dispatch graph ([ADR 0019](../adr/0019-graph-local-shard-id-and-index-clusters.md)); `ShardId(0)` in standalone.
- `local_vertex_id` — dense CSR id on that shard (`PostingHit.vertex_id` in `graph-kernel`).

An index canister holds postings for shards attached to **one graph's index cluster**; `shard_id` tags the owning graph shard without embedding Principals or `GraphId` in posting keys. Router read paths derive lookup targets from **live shard registry** and filter hits to registered shards.

**Invariant:** Postings reflect **live** property values. DML on graph shards enqueues `posting_insert` / `posting_remove` (`graph/src/index/pending.rs`). Vertex/property delete removes postings; index read APIs do not consult graph tombstones. See [../sharding/standalone-mode.md](../sharding/standalone-mode.md).

## Read APIs

| API                              | Status      | Role                                                                                                                                                                                                                                                                                                               |
| -------------------------------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `lookup_equal`                   | Implemented | Equality postings for one `(property_id, value)` — full bucket (small buckets / tests)                                                                                                                                                                                                                             |
| `lookup_range`                   | Implemented | Range over encoded values for one property — full range (small ranges / tests)                                                                                                                                                                                                                                     |
| `lookup_equal_page`              | Implemented | Paginated equality export (`after` + `limit`); the seed-routing read path                                                                                                                                                                                                                                          |
| `lookup_range_page`              | Implemented | Paginated range export over encoded values (`after` + `limit`)                                                                                                                                                                                                                                                     |
| `lookup_edge_equal_page`         | Implemented | Paginated edge equality export (`after` + `limit`)                                                                                                                                                                                                                                                                 |
| `lookup_edge_range_page`         | Implemented | Paginated ordered edge range export over encoded values (`PostingRangeRequest` bounds, optional wire-label sieve, `after` + `limit`)                                                                                                                                                                                |
| `lookup_equal_batch`             | Implemented | Batched equality export for many vertex buckets in one call: bucket-granular response admission under the shared inter-canister payload budget, `next` resume cursor (slice `specs[next..]`), per-bucket `done`/cursor continuation through `lookup_equal_page`; the seed-routing fast path for multi-anchor items |
| `lookup_edge_equal_batch`        | Implemented | Batched edge equality export with the same bucket-granular budget admission and `next` resume contract as `lookup_equal_batch`                                                                                                                                                                                     |
| `lookup_intersection`            | Implemented | Intersect multiple equality arms (edge/mixed; vertex-only is streamed via `lookup_intersection_page` — [lookup-intersection.md](lookup-intersection.md))                                                                                                                                                           |
| `lookup_intersection_page`       | Implemented | Paginated all-vertex equality intersection (`after` + `limit`): server-side walk-arm page + in-heap merge-join sieve; the streamed vertex-intersection read path                                                                                                                                                   |
| `lookup_range_intersection_page` | Implemented | Paginated range-walk plus one to eight equality sieves (`after` + `limit`): server-side finite range page filtered by up to eight vertex equality arms on distinct properties; the mixed equality-plus-range read path for ADR 0034 Slice 14                                                                       |
| `count_postings_by_value`        | Implemented | Walk one property bucket; return `(encoded_value, count)` groups ([ADR 0003](../adr/0003-federated-aggregate-merge.md))                                                                                                                                                                                            |

All read APIs run entirely inside graph-index (no graph canister calls).

**No full-bucket heap materialization invariant** ([capacity-planning.md](capacity-planning.md):
"Query paths must not materialize full buckets in heap"). Query consumers read postings through the
paginated `*_page` APIs, which return at most `limit` hits plus a resume cursor — bounding the
per-message heap on the index canister. The router (`RouterIndexLookup` /
`RouterIndexClient::lookup_equal_page` / `lookup_edge_equal_page`) and the standalone graph client
(`IcPropertyIndexClient`) loop pages for `lookup_equal` / `lookup_range` / `lookup_edge_equal`. The
non-paginated `lookup_equal` / `lookup_range` / `lookup_edge_equal` endpoints are retained for small
buckets and tests, mirroring `lookup_label` vs `lookup_label_page` ([label-index.md](label-index.md)
path A). **All-vertex equality intersection** (the planner's `IndexIntersection`) is now **streamed**:
consumers loop the server-side `lookup_intersection_page`, which walks the first arm one page at a
time and sieves the rest in-heap via a bounded merge-join, so no arm's full bucket is materialized and
the walk + sieve fold into one message per page. **Edge / mixed** intersection still builds per-arm
posting sets in heap server-side — see
[lookup-intersection.md](lookup-intersection.md#streaming-intersection-status).

**Planned:** `count_postings_by_value_for_label` — same bucket walk with label membership sieve
per posting ([label-index.md](label-index.md) Tier 3).

## Router seed routing (current)

**Partially Implemented** in `dispatch_plan_blob` (`router/src/gql.rs`):

1. `SeedProbe::from_plans` detects an equality `IndexScan` anchor only.
2. Router queries index with property id + encoded value.
3. Hits grouped by `shard_id` → one `execute_plan_on_graph` per shard.
4. `seeds_for_local_shard` builds `seed_bindings_blob` for local vertex ids.

If probe is **None**:

- **Single shard** — execute on that shard without seeds.
- **Multiple shards** — error: `no index anchor: single-shard graph required`.

**Implemented:** `IndexIntersection` anchors via `IndexAnchor::from_plans` and `lookup_intersection` with the same per-shard slice as `IndexScan`.

**`!=` predicates never form anchors.** `CmpOp::Ne` is served by residual filtering over whatever scan produced candidates; this is recorded policy, not an omission — see [ADR 0072](../adr/0072-ne-complement-index-policy.md) for the decision, its NULL/three-valued-logic invariant, and the triggers that would reopen it.

## Target: Router owns index reads

In the federation target, **graph shards do not call the index on the query hot path**. Router performs `lookup_equal` / `lookup_intersection`, slices `PostingHit` by `shard_id`, and passes seeds to each graph shard.

Legacy: graph executor still calls `PropertyIndexLookup` for `IndexScan` / `IndexIntersection` during the standalone transition.

## Graph shard local indexes

**Implemented (ADR 0009 phase D):** edge property equality postings live on **graph-index** with key
`(property_id, value, label_id, shard_id, owner_vertex_id, slot_index)`. Graph shard retains
`EDGE_PROPERTIES` as canonical values; `indexed_edge_equality` / `EdgeIndexScan` expand paths use
router seeds, a graph-index client when present, or a registered-property scan of `EDGE_PROPERTIES`
in standalone/library mode. Postings are maintained **only for administrator-registered**
properties (see ADR 0009 §2).

## Indexability vs primary storage

Primary vertex and edge property maps persist [`gleaph_gql::Value`] with [`Value::to_binary_bytes`].
Index postings use a separate **sortable index key** from [`gleaph_gql::value_to_index_key_bytes`].

Graph centralizes both paths in `crates/graph/src/property/`:

| Function                                       | Role                                                                |
| ---------------------------------------------- | ------------------------------------------------------------------- |
| `ensure_persistable`                           | Primary-store write validation                                      |
| `property_indexability` / `sortable_index_key` | Whether a value gets equality/range postings                        |
| `dispatch_property_index_ops`                  | Routes derived ops to federated vertex index or local edge equality |

**Index-only miss:** A value can be stored but omitted from indexes when `property_indexability`
is `NotIndexable` (non-finite floats, unsupported composite shapes, extensions without a sortable
key, or encoded index key length above `MAX_INDEX_VALUE_KEY_BYTES` — see
[capacity-planning.md](capacity-planning.md)) or `Absent` (null). Equality and range scans will not
find those vertices or edges until a full scan path is used. Router and graph reject oversized query
keys before graph-index calls; graph-index read APIs reject them as `IndexValueKeyTooLarge` (no
silent empty range).

## Vector search filter membership (ADR 0034 Slices 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18 and 19)

Property-index postings own filter membership for leading and non-leading `SEARCH ... WHERE`
predicates. The Router consumes postings through bounded pagination:

- The planner accepts one same-binding equality predicate, one to eight `AND`-connected
  same-binding equality predicates on **distinct** properties, exactly one same-binding one-sided range
  predicate (`<`, `<=`, `>`, `>=`), exactly two same-property range predicates forming one lower
  and one upper bound, one to eight equality predicates on distinct properties together with
  one one- or two-sided range predicate on a distinct property, or any number of
  `OR`-connected same-binding comparison predicates where each leaf is independently an equality
  or a one-sided range comparison, and preserves the original filter expression in
  `PlanOp::Search`. Range literal domains are gated at resolution (Router) and execution
  (graph executor), not by planner shape validation.
- The Router resolves the searched label and every filter property to router-issued ids and proves
  an active vertex property index for the exact `(graph_id, label_id, property_id)` tuple in the
  named-index catalog for every arm. For a leading search the label is taken from the leading
  labeled `NodeScan`. For a non-leading search the label is proved from the top-level prefix: a
  labeled `NodeScan` for the searched binding, or a `PropertyFilter`/`ExpandFilter` carrying
  `IS LABELED(binding, label, negated = false)` before the `PlanOp::Search`.
- For equality arms it encodes every comparison value with `gleaph_gql::value_to_index_key_bytes`
  and validates each encoded size against `MAX_INDEX_VALUE_KEY_BYTES` before calling the index.
  For one equality arm it pages through `lookup_equal_page` for `(property_id, encoded_value)`. For
  two to eight equality arms it constructs vertex-only `IndexEqualSpec`s and pages through the
  server-side `lookup_intersection_page`, which canonicalises the walk arm by `(property_id,
encoded_value)` order and sieves the remaining arms in heap without materializing full buckets.
  Nine or more equality arms are rejected with `InvalidArgument`.
- For one range arm it derives a finite half-open encoded comparison-domain interval with the
  canonical `gleaph_gql::range_bounds` helper (see the comparison-domain bounds contract above),
  rejects unsupported domains (`UnsupportedRangeDomain`) with an explicit error, validates each
  bound size against `MAX_INDEX_VALUE_KEY_BYTES`, and pages through `lookup_range_page` with
  `PostingRangeRequest::Between { low, high }`. For two same-property range arms the Router derives
  both half-open intervals through the same canonical helper, intersects them once (`low =
max(first.low, second.low)`, `high = min(first.high, second.high)`), validates the final bounds, and
  issues one paginated `lookup_range_page` stream with the same `PostingRangeRequest::Between`.
- For one equality arm plus one one-sided range arm on distinct properties the Router proves active
  vertex property indexes for both properties, encodes the equality value with
  `gleaph_gql::value_to_index_key_bytes`, derives a finite half-open encoded comparison-domain
  interval with `gleaph_gql::range_bounds`, and pages through `lookup_range_intersection_page`. Property
  Index walks the finite encoded range one page at a time and sieves each page against the equality
  arm server-side. The sieve is span-aware: it uses a fast dense merge scan when the page's
  `(shard_id, vertex_id)` span is small relative to its size, and falls back to page-size-bounded
  point lookups when the range-walk page scatters hits across arbitrary subject ids. The returned
  `next`/`done` always describe the range walk, so a page with zero survivors is not terminal while
  the range walk has more pages.
- For one equality arm plus two same-property range arms on a distinct property (ADR 0034 Slice 12)
  the Router performs no new Property Index operation. It reuses the same `lookup_range_intersection_page`
  path: the two range arms are collapsed into one intersected finite half-open encoded interval by the
  same `resolve_filtered_range_interval` logic used for Slice 10, the equality value is encoded once,
  and one paginated `lookup_range_intersection_page` stream walks the interval and sieves each page by
  the equality arm.
- For two to eight same-property or cross-property equality arms inside an `OR` the candidate set is
  the union of paginated `lookup_equal_page` results for every distinct `(property_id, encoded_value)`
  source, label-filtered per page and deduplicated globally by `(shard_id, vertex_id)`.
- For two to eight same-property or cross-property one-sided range arms inside an `OR` the
  candidate set is the union of paginated `lookup_range_page` results. Each arm resolves one
  `(graph_id, label_id, property_id)` tuple, derives a finite half-open encoded interval through
  the canonical comparison-domain helper, and contributes to a per-property-id merged interval set
  before lookup. The same per-page label filtering, global `(shard_id, vertex_id)` deduplication,
  and 4096 candidate bound apply.
- For two to eight same-binding heterogeneous equality/range arms inside an `OR` (ADR 0034 Slice 19)
  each arm is classified independently as equality or range, resolves one
  `(graph_id, label_id, property_id)` tuple, and is normalized using the same equality-deduplication
  and per-property range-merge rules as the pure paths. The candidate set is the union of paginated
  `lookup_equal_page` and `lookup_range_page` results for the combined normalized sources, with the
  same per-page label filtering, global `(shard_id, vertex_id)` deduplication, and 4096 candidate
  bound. Equality and range sources are never merged with each other because they are semantically
  distinct postings lookups.
- If the clamped interval is empty (`low >= high`) the Router returns an empty candidate set before
  calling the Property Index or Vector Index, preserving the empty-candidate dispatch contract.
- In all cases the Router deduplicates by `(shard_id, vertex_id)` and stops as soon as a 4097th
  distinct subject is observed. Exceeding the bound returns an explicit
   `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` error. The Property Index validates bounds structurally
   (`low < high`, key sizes) and scans only the opaque encoded interval inside the requested property
   bucket; it does not interpret GQL value types or comparison ordering.
- The candidate set is intersected with the searched vertex label on each page using
  `filter_hits_by_label`, so vertices that do not belong to the searched label do not consume the
  candidate bound.
- Postings are typed as vertex subjects by the graph-index API; the Router’s allowlist carries only
  `(shard_id, vertex_id)` tuples. Vector hits for shards that are no longer live are ignored downstream
  by the Router while it builds per-shard search seeds (leading) or lowers resolved search relations
  (non-leading). The Vector Index does not know live topology and therefore does not perform this filtering.
- The resulting allowlist is passed to Vector Index as a transient `candidate_subjects` field on the
  internal `VectorSearchRequest`. No new stable region is added and no property values are duplicated in
  the vector canister.

This follows the existing derived-state contract: postings may lag canonical vertex properties, and the search read path treats those postings as the source of filter membership. The query does not re-verify the equality property against canonical vertex properties at runtime; results are therefore eventually consistent with the primary store and may temporarily include vertices whose property has changed or omit vertices whose posting is still pending.

## Index maintenance

On DML / property updates, graph enqueues posting changes when federation routing and an index client are configured. Without client, mutations may drop index updates (`index/pending.rs`) — deployments with property indexes must wire the index canister.

**Vertex membership resolution (single owner):** single DML, bulk insert, and backfill all resolve
their namespaces through `vertex_index_memberships_for_labels`
(`crates/graph/src/index/catalog_context.rs`), so a committed transition maintains exactly the
namespaces its posting state was built against. A vertex that carries no canonical label maintains
every namespace indexing the property (legacy-unlabeled contract — it cannot be excluded from any
label scope, mirroring the Router's label-less superset anchor lookups, which serve such postings
without a sieve); labeled vertices maintain wildcard (`label_id == 0`) memberships plus exact
label matches, and label scopes that do not intersect their labels stay excluded even when they
index the same property. A missing vertex row dispatches nothing.

**Backfill:** `backfill_vertex_property_postings` on graph shards replays indexable vertex properties from
`VERTEX_PROPERTIES` into graph-index via the budget-driven `posting_batch` transport when the concrete
client supports it, with per-posting fallback for native/legacy clients (router-guarded update, same
cursor batching model as `backfill_label_postings`). Unindexable values are skipped (see `property_indexability` in
`crates/graph/src/property/`). Router orchestrates per-shard cursors via
`admin_vertex_property_backfill_step` / `admin_list_vertex_property_backfill_status` (Admin-only).

These existing operator-driven backfill endpoints are not an activation gate and do not cover the
single vertex/sidecar/`INLINE` export contract. The migration lifecycle, generation fence, short
seal, and Active-only planner publication are implemented in [ADR 0059](../adr/0059-create-index-migration-backfill.md)
for migration-driven index creation; the cross-canister PocketIC convergence/fence/upgrade-reopen
proof landed (GAP-2026-07-29-006, closed 2026-08-22), leaving edge `INLINE` enumeration open under
GAP-2026-07-29-001.

**Label-transition admission:** A label gain or loss can change eligibility for an already-present
vertex property. `facade/store/labels.rs` owns this transition: it reads canonical labels and
property values once, applies one exact label-plus-property membership filter, preflights every
affected namespace, commits Building `BuildDml` before the canonical label sidecar, and dispatches
only Active property work. A Sealing membership rejects before canonical or derived state changes;
the label posting itself remains on `label_pending`. This is a Graph-local admission contract, not
a claim that the migration's cross-canister or upgrade proof has run.

**Durable repair journal (ADR 0023 D5):** the happy-path flush stays volatile and persists nothing.
When a flush fails, the batch is compensated and persisted to the stable `INDEX_REPAIR_JOURNAL`
(`graph/src/index/repair_journal.rs`, `MemoryId 41`) instead of the volatile queue, so the
store-ahead/index-behind delta survives the upgrade boundary, the router-less timer context, and
traps. The maintenance driver re-applies journal entries each tick and on `post_upgrade`, removing
each once graph-index accepts it; re-application is idempotent so no compensation is needed on the
drain path. If compensation itself fails, the canister no longer traps (P4) — the full batch is
journaled all the same, since idempotent re-application converges the index to the store regardless
of the partial compensation state.

When a maintenance tick finds an existing repair journal, newer volatile vertex, edge, and label
pending operations are appended to its durable tail before draining. This preserves journal-before-
pending ordering and lets contiguous compatible operations share the existing bounded batch call;
pending work is never removed from volatile state without first becoming durable.
The pending queues themselves remain heap-only; an upgrade before a maintenance pass takes them
into the journal cannot recover that not-yet-journaled work, so upgrade safety begins at the durable
journal boundary rather than at enqueue time.
Plan 0088 now provides a separate durable derived-index outbox storage region and connects the
Router→Graph wire-DML finalization handoff to it. Maintenance passes the durable suffix to the
index batch endpoint, which owns the instruction budget and returns the largest acknowledged
prefix; the shared dispatcher removes only that prefix. The encoded `(shard_id, operations)`
argument is also capped at the conservative 2 MiB cross-subnet request limit, so the same Graph
code remains safe if the index canister moves subnets. DML-only ad-hoc/native blocks use the same
outbox; a block that reads after DML retains a legacy flush boundary for read-after-write visibility.

**`DROP INDEX` posting purge + durable retirement (ADR 0023 D6, implemented):** dropping an index
removes the dropped property's postings from graph-index, not just the router catalog entry
(closing P7, where dropped indexes orphaned their postings). The index exposes a router-guarded,
bounded, resumable `admin_purge_property_postings(kind, property_id, label_id, resume)`
(`graph-index/src/facade/store/posting_purge.rs`) mirroring `admin_detach_shard_canister`: posting
keys order `property_id` first and each scope is namespaced by its physical id — **vertex** keys
carry no label (purge the whole `(physical_index_id, property_id)` range); **edge** keys carry the
catalog `label_id` (direction stripped), so the purge filters to `(physical_index_id, property_id,
label_id)`.

Because posting ranges are disjoint per physical namespace and no reader can reach a namespace
whose catalog row is gone, **every dropped definition retires its own physical namespace
unconditionally** — a sibling index on a shared property neither needs those postings nor protects
them. The Router co-writes the catalog removal with a durable retirement record keyed by
`PhysicalIndexId` (`ROUTER_INDEX_RETIRED`, MemoryId 54,
`router/src/facade/stable/index_retirement.rs`) whose value freezes the drain target set resolved
*before* the destructive mutation plus one resume cursor per pending target. The inline fast path
drives bounded rounds during the DDL call; any hold (stopped target, response loss, upgrade) defers
the remainder to the recovery timer's retirement-drain lane (`router/src/index_retirement.rs`,
Driver 4), which advances one bounded step per pending target per tick until every target confirms
`done` and deletes the record. PhysicalIndexId allocation is monotonic and never reused, so a
retired identity cannot be resurrected by a later `CREATE INDEX`; `DROP INDEX` therefore always
returns success after the co-write and never fails on purge transport errors. A graph with no live
index canister owns its posting cleanup through the shard detach flow instead.

## Implementation-gap traceability (non-normative)

The implementation-gap ledger is the status authority for the following open
observations. These links do not amend the property-index contract above.

- [GAP-2026-08-20-005](../implementation-gaps.md#gap-2026-08-20-005--property-drop-index-has-no-durable-per-physicalindexid-retirement-lifecycle) — **Resolved (commit pending)**: per-`PhysicalIndexId` DROP retirement durability; owning regressions `dropping_one_of_two_indexes_on_shared_property_does_not_leak_its_namespace` + router unit tests in `index_retirement.rs`.
- [GAP-2026-08-20-006](../implementation-gaps.md#gap-2026-08-20-006--router-shard-identity-has-no-incarnation-or-lifecycle-fence) — **Open**: shard incarnation and unregister lifecycle fencing.

## Derived-state lag

See [derived-state-query-semantics.md](derived-state-query-semantics.md) for query behavior when
pending flush, backfill, or index unavailability leaves postings behind canonical properties.

## Related documents

- [../adr/0006-pre-federation-foundation.md](../adr/0006-pre-federation-foundation.md) — router property catalog SSOT
- [../adr/0059-create-index-migration-backfill.md](../adr/0059-create-index-migration-backfill.md) — partially implemented migration-driven online index backfill and activation (production driver implemented; E2E validation pending)
- [derived-state-query-semantics.md](derived-state-query-semantics.md)
- [label-index.md](label-index.md) — vertex label membership; tiered reads with property index ([ADR 0004](../adr/0004-label-index.md))
- [lookup-intersection.md](lookup-intersection.md)
- [../sharding/standalone-mode.md](../sharding/standalone-mode.md)
- [../sharding/federation-target.md](../sharding/federation-target.md)
- [capacity-planning.md](capacity-planning.md) — 500 GiB headroom, split thresholds, planned inverted posting lists
- [../architecture/overview.md](../architecture/overview.md)
- [../federation/query-semantics.md](../federation/query-semantics.md)
- [../gql/plan-format.md](../gql/plan-format.md)
