# Discovered Implementation Gaps

Last updated: 2026-08-03
Anchor timestamp: 2026-08-03 08:24:24 UTC +0000

## Status

**Active tracking document** — this ledger records implementation defects, missing product
capabilities, and contract mismatches discovered while implementing another slice when they cannot
be resolved safely in that slice.

It is not a second roadmap or design source of truth. Each entry names the owning module and links
the active design contract. Once an architectural decision is accepted, the owning design document
or ADR remains authoritative and this ledger points to it.

## Disposition rule

Every counterpartrial gap discovered during implementation, review, validation, or demo integration must
receive one disposition before the current work is committed:

1. **Fix now** when it is a correctness or security defect, blocks the current contract, has a clear
   owner, and can be repaired without obscuring the current slice.
2. **Prerequisite slice** when it blocks the current work but needs independent implementation,
   review, validation, or commit history.
3. **Record here** when it is real but non-blocking, its design is unresolved, or fixing it would
   expand the current slice counterpartrially.
4. **Reject as not a gap** only with evidence that the observed behavior matches an existing active
   contract.

Do not leave a gap only in terminal scrollback, a temporary report, an ignored plan file, or a final
chat summary.

## Entry requirements

Each open entry must state:

- **Observed behavior:** reproducible fact, not a proposed solution;
- **Expected or needed behavior:** the contract or product need that exposes the gap;
- **Owner:** module/domain that owns the violated invariant or missing API surface;
- **Evidence:** test, command, source path, or design section;
- **Impact:** what remains unsafe, impossible, misleading, or inefficient;
- **Next decision:** the smallest question or slice that can resolve it;
- **Status:** `Open`, `Planned`, `In progress`, `Resolved`, or `Not a gap`.

Resolved entries remain in the ledger with the fixing commit and owning test. This prevents the same
defect from being rediscovered without its prior reasoning.

## Open gaps

### GAP-2026-07-25-002 — Tombstone-heavy OFFSET scans lack a persistent skip structure

- **Status:** Planned
- **Severity:** P2 traversal performance and stable-layout research gap
- **Owner:** `ic-stable-lara` traversal and LARA logical-slot metadata
- **Observed behavior:** `TraversalWindow.offset` can jump directly only for a proven dense bucket.
  Sparse or tombstone-bearing buckets inspect logical rows to count live matches before delivering
  the requested window. No persistent interval summary, live bitmap, or rank/select structure is
  currently part of the LARA layout.
- **Expected or needed behavior:** A future implementation may skip tombstone-only regions and
  resolve the requested live ordinal without inspecting every preceding logical slot, while
  preserving logical-slot identity, exact forward/reverse ordering, overflow semantics, and
  fail-closed corruption handling.
- **Evidence:** [ADR 0050](adr/0050-lara-traverse-read-api.md) § “Known gap: tombstone-aware offset
  acceleration”; `TraversalWindow` and the labeled sparse traversal implementation. The current
  dense fast path is intentionally limited to `live_degree == logical_extent`.
- **Impact:** Large sparse buckets can spend instructions scanning tombstones for OFFSET/LIMIT.
  Introducing durable metadata now would expand the storage format and add mutation,
  compaction/rebuild, reopen, and benchmark obligations before the design is understood.
- **Next decision:** Conduct a bounded research and benchmark slice comparing fixed-width block
  live-count summaries, hierarchical summaries, and live-bitvector rank/select. Only after an
  accepted design establishes consistency and measured benefit should a persistent structure be
  proposed in a follow-up ADR.

### GAP-2026-07-25-001 — Paired canonical edge writes expose recoverable post-write errors

- **Status:** Resolved by Plan 0182
- **Severity:** P1 canonical consistency risk
- **Owner:** `ic-stable-lara` bidirectional owner and GraphStore paired mutation boundary
- **Observed behavior:** The owner writes paired halves sequentially. The implementation traps on
  reverse-half and post-write maintenance-admission failures, and dirty-key admission restores its
  bitmap bit when queue append fails. Deterministic failure-injection tests cover directed,
  undirected, and maintenance-queue paths.
- **Expected or needed behavior:** After the first canonical half is written, the supported
  paired-mutation boundary must either complete both halves and all required mutation intent, or
  trap/rollback the whole message segment. A recoverable `Err` must not expose a partially updated
  bidirectional graph to a caller that handles the result without trapping.
- **Evidence:** `crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs`
  (`insert_directed_edge_with_locations`, `insert_undirected_deferred_with_locations`) and
  `design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md` § “Shard-local
  preflight and commit discipline”. Graph reverse-adjacency tests intentionally inject and repair
  forward-only and reverse-only states, proving that the state is observable when the paired
  boundary is bypassed.
- **Impact:** A lower-level caller or a handler that returns an ordinary error could leave forward
  and reverse canonical adjacency out of sync, while the counterpart sidecar is already invalidated.
- **Next decision:** Keep exact counterpart validation for repair and published lookup. Any future
  mutation path must preserve the same preflight-then-trap boundary; a new recoverable post-write
  error requires a separate atomicity design review.

### GAP-2026-07-17-001 — Dynamic instruction-budget headrooms lack measured acceptance criteria

- **Status:** Open
- **Severity:** P1 availability and resumability risk
- **Owner:** Router, Graph, graph-index, and graph-vector-index dynamic batch boundaries
- **Observed behavior:** Multiple canister paths stop before the 40B update-call limit using
  independently chosen values, but no repository benchmark or acceptance table establishes that
  each headroom covers its owning path's final operation, response encoding, post-operation drain,
  and cross-canister call overhead. Current examples include Router's 5B headroom, Graph's pending
  5B headroom for `execute_plan_update_batch`, graph-index and graph-vector-index batch ceilings at
  32B with 100M per-loop reserves, graph-index's 1B update / 0.5B query lookahead, and Graph timer
  maintenance at 32B with a 100M reserve. These values have different ownership and semantics and
  must not be treated as interchangeable.
- **Expected or needed behavior:** Every dynamic batch or bounded maintenance loop must have a
  measured, path-specific instruction ceiling and headroom acceptance criterion. A boundary must
  return continuation progress before the platform limit, including the cost of its final operation,
  response construction, and owned post-processing. A single operation that cannot fit within the
  safe ceiling must fail with an explicit bounded-work error or be split at an inner owner boundary;
  it must not trap by crossing the 40B message limit.
- **Owner:** The canister that owns each loop and continuation contract: Router for Router mutation
  batching, Graph for Graph-plan batching and Graph-local drain, graph-index for posting batches,
  and graph-vector-index for vector sync batches. `design/adr/0042-router-dynamic-instruction-budget-batching.md`
  describes the API contract but is not evidence that its numeric headroom is sufficient.
- **Evidence:** `crates/router/src/lib.rs` (`MAX_DYNAMIC_INSTRUCTION_BUDGET`),
  `crates/graph/src/canister/handlers.rs` (`execute_plan_batch`),
  `crates/graph-index/src/canister.rs` (`posting_batch`),
  `crates/graph-vector-index/src/canister.rs` (`vector_sync_batch`),
  `crates/graph/src/facade/ic_budget.rs` (Graph timer budgets), and the local social-demo failure
  where `execute_plan_update_batch` reached `IC0522` at 40,000,000,000 instructions before a
  continuation cursor could be returned.
- **Impact:** An apparently dynamic batch can still trap instead of returning `next_index`, leaving
  the caller unable to make progress through the normal cursor path. Conversely, excessive
  unmeasured headroom reduces throughput and increases the number of ingress calls without a
  defensible safety benefit.
- **Next decision:** Add focused canbench and, where required, one bounded PocketIC boundary test
  per owner path. Measure the maximum single-operation cost and tail cost across representative and
  adversarial fixtures, define an explicit safety percentile or worst-case bound, then set each
  headroom independently. Record the resulting values and acceptance evidence in the owning ADR or
  design document; do not justify a numeric value solely by copying another canister's constant or
  citing an existing ADR statement.
- **Related contracts:** [ADR 0041](adr/0041-router-graph-batch-mutation-dispatch.md),
  [ADR 0042](adr/0042-router-dynamic-instruction-budget-batching.md),
  [ADR 0020](adr/0020-deferred-maintenance-timer-drain.md),
  [design/index/property-index.md](index/property-index.md),
  [design/index/vector-index.md](index/vector-index.md)

### GAP-2026-07-11-005 — `FreeSpanStore` reopen validation has no production-scale cost bound

- **Status:** Open
- **Severity:** P2 operational scalability risk
- **Owner:** `ic-stable-lara` free-span persistence and Graph upgrade preflight
- **Observed behavior:** `FreeSpanStore::init` validates the records/bin ↔ `by_start` bijection by
  collecting all active `(start_slot, id)` pairs, sorting them in heap, and comparing them with the
  ordered index. The algorithm is `O(active log active)` with `O(active)` transient heap. Existing
  canbench coverage measures at most 4,096 active spans.
- **Expected or needed behavior:** before production durable upgrades are claimed, the owner must
  define and measure a maximum supported fragmentation level, reopen instruction ceiling, and
  transient-heap ceiling. Reopen must remain fail-closed; performance work must not weaken the
  bin/index bijection.
- **Evidence:** `crates/ic-stable-lara/src/lara/edge/free_span.rs::validate`;
  `crates/ic-stable-lara/src/lara/edge/free_span/bench.rs::bench_reopen`;
  [storage/lara.md](storage/lara.md) “Reopen integrity”; and ADR 0039 “Upgrade preflight” plus
  “Performance and capacity gates”.
- **Impact:** a highly fragmented long-lived graph could make `post_upgrade` exceed its instruction
  or heap budget even though its stable layout is valid, preventing the new Wasm from serving.
- **Next decision:** first add scale-probing benchmarks beyond 4,096 spans and predeclare acceptance
  limits. If the current validator exceeds them, compare bounded incremental validation, persisted
  validation summaries with generation fencing, and an explicit hard fragmentation cap. Any change
  to the fail-closed validation contract requires an ADR 0007/0039 amendment.
- **Related contracts:** [ADR 0007](adr/0007-stable-memory-layout.md),
  [ADR 0039](adr/0039-production-stable-memory-evolution-and-upgrade-safety.md),
  [storage/lara.md](storage/lara.md)

### GAP-2026-07-11-004 — Non-tail homogeneous bypass insertion rewrites successor origins in `O(V)`

- **Status:** Open
- **Severity:** P2 performance risk
- **Owner:** `ic-stable-lara` labeled adjacency geometry
- **Observed behavior:** after a homogeneous bypass edge insert,
  `bump_successor_origins_after_bypass_end` scans every later vertex row and rewrites each later
  bypass origin that falls before the new region end. A bypass vertex that ceases to be the tail as
  more vertices are appended can therefore make one later edge insert proportional to the number of
  successor vertices.
- **Expected or needed behavior:** insertion cost must remain bounded by the owning PMA leaf/segment
  or the system must explicitly prohibit or promote non-tail bypass rows before they enter this hot
  path. Scan semantics and the direct vertex-row lookup contract must remain unchanged unless a
  reviewed representation decision replaces them.
- **Evidence:** `crates/ic-stable-lara/src/labeled/graph/bypass.rs::insert_homogeneous_bypass_edge`
  and `bump_successor_origins_after_bypass_end`; existing bypass canbench coverage does not isolate
  repeated inserts into an old bypass vertex across increasing successor counts.
- **Impact:** repeated insertion into an early bypass vertex can grow toward `O(EV)` work and stable
  row writes, creating an instruction-limit cliff not represented by current benchmark gates.
- **Next decision:** add vertex-count scaling benchmarks first. Prefer an existing leaf-owned
  geometry update or eager promotion if it meets the measured bound. Write a dedicated adjacency
  representation ADR only if the chosen fix changes persisted row meaning, scan geometry, or PMA
  ownership; a local bounded optimization needs only a plan plus design/benchmark sync.
- **Related contracts:** [ADR 0001](adr/0001-labeled-segment-slide.md),
  [ADR 0022](adr/0022-degree-driven-hub-edge-storage.md), [storage/lara.md](storage/lara.md)

### GAP-2026-07-31-001 - Prepared sort variants are not retained in the heap cache

- **Status:** Open
- **Severity:** P2 prepared-query performance gap
- **Owner:** Router prepared-query heap cache
- **Observed behavior:** The Router keeps the unsorted prepared plan in the heap cache. When a caller supplies a non-empty sort specification, the Router validates the metadata and rebuilds a derived plan for that invocation; equivalent sort specifications are not cached as separate heap entries.
- **Expected or needed behavior:** Repeated executions of the same prepared operation with the same normalized sort specification should reuse a heap-cached derived plan while stable storage continues to retain only the query source and metadata.
- **Evidence:** crates/router/src/prepared.rs::prepare_sorted_cache and the prepared query local E2E in crates/codegen/e2e/test.mjs.
- **Impact:** Sort-enabled prepared queries pay the planning and plan-encoding cost on every execution. This preserves correctness and upgrade safety but can increase latency and instruction usage for frequently reused sort variants.
- **Next decision:** Define a bounded cache key and eviction policy, including direction normalization, key ordering, metadata changes, and post-upgrade invalidation, then add hit/miss tests and a focused benchmark before introducing durable or unbounded derived state.
- **Related contracts:** ADR 0053 and crates/prepared-api/src/lib.rs

### GAP-2026-07-04-001 — Prepared execution still requires graph visibility

- **Status:** Open
- **Severity:** P2 product gap; P1 if a public frontend must call Router prepared queries directly
- **Owner:** Router prepared catalog resolution and graph authorization
- **Observed behavior:** `authorize_prepared_execute` permits the default Router `Executor` role,
  including an anonymous caller, but prepared-plan resolution searches only graphs visible to the
  caller. A principal that is not the graph owner or in the graph `admins` set therefore cannot
  resolve the prepared plan. The social demo test must currently add its application caller to the
  graph administrators while leaving its Router role at `Executor`.
- **Expected or needed behavior:** an application should be able to expose an administrator-registered
  read-only prepared query without granting its calling principal graph-administrator membership,
  or the product contract must explicitly require an application backend principal with graph
  visibility.
- **Evidence:** `crates/router/src/rbac.rs::authorize_prepared_execute`,
  `crates/router/src/prepared.rs::resolve_prepared_graph_id`, and the shared
  `install_single_shard_federation_with_graph_admins` fixture in
  `crates/pocket-ic-tests/src/lib.rs`.
- **Impact:** the initial public social demo cannot truthfully claim direct anonymous prepared-query
  execution. The current bounded workaround is a graph-visible application principal with no Router
  ad-hoc `Read` role; anonymous and default-Executor semantics must not be conflated.
- **Next decision:** assess three existing-boundary alternatives before adding an API: application
  backend canister principal with graph visibility; a graph-level read/execute membership distinct
  from administrators; or a prepared-plan public-execution flag whose graph is resolved at
  registration. If the latter two are chosen, write an authorization ADR and adversarial cross-graph
  tests.
- **Implemented workaround (does not close this gap):** `crates/social-demo-gateway` provides an
  application-owned canister with a fixed three-variant scenario enum. The Gateway principal is
  registered as a graph administrator so Router can resolve the prepared plan, but it remains a
  default Router Executor with no ad-hoc `Read` role. Anonymous callers execute the fixed scenarios
  through the Gateway; Router observes the Gateway principal, not the original caller. This is an
  application-layer trusted-deputy pattern, not a product change to Router prepared-query
  authorization.
- **Related contracts:** [security/rbac-and-prepared.md](security/rbac-and-prepared.md),
  [demo/social-graph-rag.md](demo/social-graph-rag.md)

### GAP-2026-08-01-001 — Social-demo load artifacts were not reproducible from build-config.mjs

- **Status:** Resolved by Plan 0204 on 2026-08-02 UTC
- **Severity:** P2 demo-tooling drift; blocks trusting `pnpm run build:config` as an idempotent
  regeneration step
- **Owner:** `frontend/apps/social-demo/scripts/build-config.mjs` and the committed seed/avatar
  artifacts
- **Resolution:** The generator now emits one ordered `social-load.json` directly from the canonical
  node/edge model. Its typed properties contain no execution-time values, application endpoint keys
  remain explicit, and exact UTC timestamps are deterministic. Focused contracts cover source-ID
  closure and byte-stable generation inputs; the durable loader uses the full artifact SHA-256 plus
  exact Router chunk replay rather than parsing generated mutation text.

### GAP-2026-08-03-001 — Router seal activation crash window can strand an already-Active Graph export scope

- **Status:** Resolved by ADR 0059 seal-step tolerance (commit in the same patch)
- **Severity:** P2 crash-window recovery gap (narrow, non-blocking for first rollout)
- **Owner:** `crates/router/src/facade/store/schema_migration/driver.rs` seal composition plus Graph
  `canonical_export` Active-scope semantics
- **Observed behavior:** In one seal drive the driver publishes every Graph export scope
  (`admin_activate_index_export_scope`) as its final remote step. If the Router message traps or
  rolls back after those activations but before persisting the `Converged` target, a re-drive's
  step-1 `admin_seal_index_export_scope` returned `InvalidPhase` because the scope is already
  `Active`; the driver classified that as terminal, the router entered `Aborting`, and Graph
  rejected `admin_abort_index_export_scope`/`admin_remove_index_export_scope` for an `Active`
  scope, so cleanup could not progress.
- **Resolution:** `seal_scope` now treats an already-`Active` scope under the exact same frozen
  identity and lifecycle epoch as an exact replay and returns the durable status instead of
  `InvalidPhase`; `activate_scope` already replays an `Active` scope whose proof matches. The
  re-drive therefore re-seals (replay), re-reads the graph-index seal proof (idempotent),
  drains (already converged), and re-activates (idempotent) without error, so the Router
  persists `Converged` and completes normally. A different epoch or identity still fails closed,
  and `Active` remains deliberately non-abortable and non-removable.
- **Evidence:** `crates/router/src/facade/store/schema_migration/driver.rs` `drive_seal`;
  `crates/graph/src/index/canonical_export.rs` `seal_scope`/`activate_scope`/`abort_scope`/`remove_scope`;
  ADR 0059 seal ordering.
- **Regression tests:** `seal_scope_replays_already_active_scope_after_activation_crash_window`
  (graph lib) and `driver_seal_resumes_after_activation_crash_window` (router driver) prove the
  re-drive converges and that a wrong `InvalidPhase` return for the same identity/epoch would fail.

## Resolved gaps

### GAP-2026-07-14-001 — Ordered incremental slab compaction repeatedly scans packed prefixes

- **Status:** Resolved by Plan 0201 (ADR 0052 Slice 6; commit in the same patch)
- **Severity:** P2 maintenance-performance risk
- **Owner:** `ic-stable-lara` labeled edge-slab compaction and deferred-maintenance cursor state
- **Observed behavior:** `compact_vertex_edge_span_one_step` preserves edge scan order and emits at
  most one `EdgeSlotMove` per queue pop, but `first_edge_slot_move_in_bucket` restarts at slot zero
  after every move. Alternating tombstones therefore make full bucket compaction quadratic in the
  resident slab width. After edge-log and inline-property maintenance were separated, canbench measured
  `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_1024` at 73.36M instructions versus
  4.14M previously; the existing 4,096- and 16,384-edge cases were already dominated by the same
  quadratic shape.
- **Expected or needed behavior:** preserve ascending/descending edge scan order and immediate
  per-edge sidecar/index re-keying while carrying enough progress state that successive bounded
  maintenance steps do not rescan the already-packed prefix.
- **Resolution:** `CompactVertexEdgeSpan` (v1 work item) carries a `resume_slot_index` cursor;
  `compact_vertex_edge_span_one_step` and both finders scan from the cursor with `next_live` seeded
  from it (packed-prefix invariant), `EdgeMoved` re-enqueues carry the advanced cursor,
  `AdvanceBucket` resets it, and a cursor-scan miss triggers a full re-scan from slot zero before
  `stored_slots = degree`, so an interleaved delete inside the already-packed prefix can never
  truncate a live edge. `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_1024` moves off
  the quadratic shape.
- **Evidence:** `crates/ic-stable-lara/src/labeled/graph/compact.rs::first_edge_slot_move_in_bucket`
  (cursor parameter), `compact_vertex_edge_span_one_step`; regression tests
  `compaction_cursor_packs_alternating_tombstones_across_steps` and
  `compaction_cursor_survives_interleaved_delete_in_packed_prefix` in
  `labeled/bidirectional/deferred.rs`; ADR 0020's one-`EdgeMoved`-per-pop contract.
- **Related contracts:** [ADR 0001](adr/0001-labeled-segment-slide.md),
  [ADR 0020](adr/0020-deferred-maintenance-timer-drain.md),
  [ADR 0052](adr/0052-per-label-adjacency-order-and-tombstone-reuse.md) Slice 6

### GAP-2026-07-04-003 — No application-facing vertex-embedding ingestion boundary

- **Status:** Resolved by plan 0048 implementation
- **Severity:** P1 product gap
- **Owner:** Router authorization/resolution + Graph canonical embedding store
- **Observed behavior:** before plan 0048, there was no canister API for an application or deployment
  tool to write a canonical vertex embedding. Vector-index fixtures and demos had to seed the
  derived `graph-vector-index` canister directly, bypassing Graph canonical ownership and the
  Router embedding-name catalog.
- **Expected or needed behavior:** an authorized caller should submit only graph name, opaque encoded
  vertex id, registered embedding name, and finite F32 values to Router; Router should resolve
  ownership and dispatch a single canonical write to the Graph shard; Graph should commit the
  canonical bytes/version and drive derived convergence; the result must distinguish a fully
  applied projection from a durable deferred repair.
- **Resolution:** plan 0048 adds the Router admin endpoint `admin_ingest_vertex_embedding` and the
  Router-only Graph endpoint `admin_ingest_vertex_embedding`. Router validates the encoded id, live
  shard, and registered vector definition before dispatch; Graph verifies vertex existence, commits
  canonical bytes through `set_vertex_embedding`, attempts `vector_pending` delivery, and returns
  `VertexEmbeddingIngestionResult { embedding_version, projection_outcome }`. Invalid inputs fail
  closed before any Graph call; projection failure defers to the existing repair journal while
  keeping the canonical write.
- **Evidence:** `crates/router/src/canister.rs::resolve_vertex_embedding_ingestion` unit tests;
  `crates/graph/src/canister/handlers.rs::vertex_embedding_ingestion_tests` unit tests;
  `crates/pocket-ic-tests/tests/adr0031_vertex_embedding_ingestion.rs` PocketIC contract proving
  canonical ingestion reaches Router vector search without direct vector-canister seeding.
- **Related contracts:** [ADR 0031](./adr/0031-vertex-embedding-store-and-derived-vector-index.md),
  [design/index/vector-index.md](./index/vector-index.md),
  [design/execution/pipeline.md](./execution/pipeline.md)

### GAP-2026-07-04-002 — `NEXT INSERT` lost edge endpoint identity

- **Status:** Resolved by commit `27e993ae`
- **Severity:** P1 correctness defect
- **Owner:** GQL block planning and Graph projection/mutation execution
- **Observed behavior:** a `MATCH ... RETURN ... NEXT INSERT (a)-[:L]->(b)` mutation reported
  success, but a later traversal observed disconnected/`NULL` endpoints. Separate seed operations
  could not build a shared-vertex social graph.
- **Resolution:** no-YIELD `NEXT` boundaries now preserve typed graph bindings; already-bound node
  variables are not planned as new vertices; plain-variable projections retain `PlanBinding`
  identity through native and wire execution.
- **Evidence:**
  `gql_run::tests::{block_match_next_insert_edge_keeps_endpoints,wire_block_match_next_insert_edge_keeps_endpoints,block_match_next_insert_edge_shares_source}`.
- **Related contracts:** [gql/plan-format.md](gql/plan-format.md),
  [execution/pipeline.md](execution/pipeline.md)

## Property and index capability gaps

The following status was verified against the implementation at the anchor timestamp above.
The canonical property-index contract remains [design/index/property-index.md](index/property-index.md);
this section records only the remaining gaps and the boundary at which each one is owned.
[ADR 0059](adr/0059-create-index-migration-backfill.md) is the accepted, partially implemented
source of truth for the migration-driven `CREATE INDEX` backfill lifecycle; this ledger does not
duplicate its state machine or ownership rules.

### Priority order

| Priority | Work item                                                                                  | Current status                                                                               |
| -------- | ------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------- |
| P0       | Backfill existing vertex, sidecar, and INLINE values before advertising an index as active | Partial — GAP-2026-07-29-006; ADR 0059 production driver implemented, E2E validation pending |
| P1       | Define INLINE removal/`NULL` transitions and complete vertex `MATCH` range planner wiring  | Planned/Open — GAP-2026-07-29-002, GAP-2026-07-29-004                                        |
| P1       | Add edge range postings, Router seed planning, and execution support                       | Open — GAP-2026-07-29-003                                                                    |
| P2       | Add vertex nested-record field indexes with a canonical dotted-path contract               | Planned — GAP-2026-07-29-005                                                                 |
| P2       | Add record/list index semantics and tests, after the scalar/leaf contract is fixed         | Planned                                                                                      |
| P3       | Decide edge-property uniqueness enforcement and multi-canister index sharding axes         | Planned                                                                                      |

The P0 item is a prerequisite for trusting any newly created index. The range premise is narrower:
the ordered scan primitive already exists through `StableBTreeMap::range()`; the remaining work is
to expose its capability at every required planner and entity boundary. Range support must not be
reimplemented as a full-bucket materialization.

### Range index status

Range traversal itself is **implemented**, not missing: `graph-index` converts the encoded bounds to
a half-open `StableBTreeMap::range()` scan, and paginated, label-sieved vertex range endpoints are
available. Router `SEARCH ... WHERE` also uses those endpoints for the supported numeric range
shapes. The missing pieces are planner coverage and edge symmetry, not the basic range scan.

### GAP-2026-07-29-001 — Existing INLINE values are not included by property-index backfill

- **Status:** Open
- **Severity:** P0 index correctness
- **Owner:** Router backfill orchestration and Graph inline-property backfill boundary
- **Observed behavior:** The edge-property backfill scans canonical `EDGE_PROPERTIES` only. Existing
  values stored in the edge inline-property region are therefore not enumerated by that backfill,
  although subsequent mutations can maintain an indexed inline property when the router supplies
  the active catalog.
- **Expected or needed behavior:** Creating or enabling an index on an eligible INLINE property must
  converge existing inline values before the index is advertised as active; the backfill cursor and
  posting owner must cover the inline storage domain as well as the sidecar domain.
- **Evidence:** `crates/graph/src/index/edge_property_backfill.rs` calls
  `scan_edge_properties_batch`; inline storage is owned by `crates/graph/src/edge_inline_property_schema.rs`
  and the edge store helpers. `design/storage/labeled-edge-inline-properties.md` describes the
  inline bytes as canonical for the declared inline property.
- **Impact:** A newly created index can miss pre-existing inline values and return incomplete
  equality/range candidates until those values are rewritten.
- **Next decision:** The [ADR 0059](adr/0059-create-index-migration-backfill.md) lifecycle (one
  Graph-owned opaque export, graph-index pull, touched-first exact outbox mutation, base-seed guard,
  and Active-only transition) is implemented; focused PocketIC E2E/upgrade validation remains
  pending. The existing operator cursor endpoints remain separate from that lifecycle.

### GAP-2026-07-29-002 — Normal `MATCH` planning does not select vertex range indexes

- **Status:** Open
- **Severity:** P1 query capability
- **Owner:** Router catalog projection and GQL planner statistics boundary
- **Observed behavior:** The graph-index range API and `SEARCH` range path are present, but
  `RouterGraphStats::is_vertex_property_range_indexed` returns `false` unconditionally. The normal
  `MATCH ... WHERE` planner therefore cannot use a vertex range index through its planner-statistics
  contract.
- **Expected or needed behavior:** The Router should project the active range-capable vertex
  properties into planner statistics, while retaining the existing fail-closed behavior for
  properties without an active index.
- **Evidence:** `crates/router/src/planner_stats.rs::is_vertex_property_range_indexed` and
  `crates/router/src/index_client.rs::lookup_range_page`; the latter is already used by
  `crates/router/src/gql_search.rs`.
- **Impact:** A range index may exist and be queryable through `SEARCH`, yet ordinary MATCH planning
  falls back to non-index execution or rejects the index-backed path.
- **Next decision:** Add range capability to the Router's active catalog projection and planner
  selection, then cover bounded one-sided and two-sided MATCH predicates with planner and execution
  tests. Keep the `StableBTreeMap::range()` storage primitive unchanged.

### GAP-2026-07-29-003 — Edge range index has no query/planner path

- **Status:** Open
- **Severity:** P1 query capability
- **Owner:** Graph-index edge posting API, Router edge seed planning, and Graph edge candidate execution
- **Observed behavior:** Edge postings support equality lookup and paged equality lookup, but no
  `lookup_edge_range` endpoint, edge range request path, or edge range planner-statistics capability
  exists. The implemented `PostingRangeRequest` path is vertex-posting oriented.
- **Expected or needed behavior:** An edge property declared indexable should support the same
  encoded ordering contract for bounded range predicates, including label/direction scoping and
  resumable pages, before the planner emits an edge range scan.
- **Evidence:** `crates/graph-index/src/facade/store/edge_postings.rs` exposes
  `lookup_edge_equal` / `lookup_edge_equal_page`; `crates/router/src/planner_stats.rs` exposes only
  edge equality membership; no `lookup_edge_range` symbol exists in `crates/graph-index` or
  `crates/router`.
- **Impact:** `COST BY e.stats.field` and edge filters can use an equality index, but a numeric range
  predicate on an edge property cannot use the existing ordered posting storage.
- **Next decision:** Reuse the existing sortable encoded value and `StableBTreeMap::range()`
  primitive, adding an edge-specific cursor/request that preserves `(label_id, shard_id,
owner_vertex_id, slot_index)` ordering and the existing direction subset rule.

### GAP-2026-07-29-004 — INLINE removal/NULL transitions need explicit posting semantics

- **Status:** Planned
- **Severity:** P1 index consistency
- **Owner:** Graph inline-property mutation and index posting dispatch
- **Observed behavior:** Inline index maintenance now supports eligible scalar values and struct leaf
  paths, but the contract for removing a field, assigning `NULL`, and changing a struct shape is not
  yet recorded as a complete old-key/new-key transition.
- **Expected or needed behavior:** Every mutation must remove the old sortable posting when the
  indexed value disappears or becomes non-indexable, and insert the new posting only after the
  inline bytes and decoded value agree. A missing/NULL value must never remain queryable as a stale
  posting.
- **Evidence:** `crates/graph/src/property/inline_dispatch.rs`,
  `crates/graph/src/facade/store/edge_profiles.rs`, and the inline-property contract in
  `design/storage/labeled-edge-inline-properties.md`.
- **Impact:** Deletes and shape changes can leave false-positive index candidates even when scalar
  replacement is correct.
- **Next decision:** Add mutation-contract tests for remove, NULL, missing nested field, and
  non-indexable replacement before widening inline index coverage.

### GAP-2026-07-29-005 — Vertex nested-record indexes are not yet symmetrical with edge INLINE fields

- **Status:** Planned
- **Severity:** P2 query capability
- **Owner:** Vertex property storage, planner property-path resolution, and property-index backfill
- **Observed behavior:** Edge INLINE struct leaf paths can be indexed, while the corresponding
  vertex nested-record field path is not a generally supported indexed property domain.
- **Expected or needed behavior:** A declared vertex index on a bounded nested field must use one
  canonical dotted path, validate the record shape, and maintain/backfill the leaf posting with the
  same old-key/new-key semantics.
- **Evidence:** Current inline field dispatch in `crates/graph/src/property/inline_dispatch.rs` and
  the vertex property index/backfill paths in `crates/graph/src/index/`.
- **Impact:** `COST BY v.stats.field` and vertex nested-field range/equality planning cannot rely on
  the same index contract as edge inline fields.
- **Next decision:** Decide whether vertex nested fields share the edge dotted-path encoding or use a
  separate record-index key domain; record/list values remain out of scope until that choice is made.

### GAP-2026-07-29-006 — Index activation convergence gate lacks production driver/E2E completion

- **Status:** Partially implemented
- **Severity:** P0 index correctness
- **Owner:** Router index catalog and backfill lifecycle
- **Observed behavior:** Router now persists hidden Preparing/Building/Sealing/Aborting lifecycle
  rows and derives planner membership only from Active rows. Graph owns exact canonical export
  scopes, graph-index owns resumable build state, the production Router cross-canister driver
  composes register/advance/seal/cleanup with unit coverage, and the CLI exact-replays one immutable
  artifact. Focused PocketIC E2E and upgrade validation are not yet complete, so the public
  migration path remains retryably fail-closed until that proof passes.
- **Expected or needed behavior:** A newly declared index must remain pending until sidecar and
  INLINE backfill has completed for every attached shard, or the query planner must explicitly treat
  it as incomplete and fail closed. Rebuild/drop must use the same lifecycle rule.
- **Evidence:** `crates/router/src/facade/store/backfill.rs`, `crates/router/src/planner_stats.rs`,
  and `design/index/property-index.md`'s router-owned active catalog description.
- **Impact:** A query can observe an index that is structurally present but incomplete, producing
  false negatives rather than merely falling back to a slower plan.
- **Next decision:** Validate the catalog-epoch seal and per-shard watermark convergence in PocketIC
  (including upgrade reopen) and keep the migration endpoint fail-closed until that proof passes.
  The real Router driver over the landed Graph and graph-index controls is implemented. [ADR 0059](adr/0059-create-index-migration-backfill.md)
  remains the source of truth.

### GAP-2026-07-29-007 — Edge uniqueness and index-canister sharding remain design work

- **Status:** Planned
- **Severity:** P3 product/capacity capability
- **Owner:** Router DDL/catalog for uniqueness; graph-index deployment and routing for sharding
- **Observed behavior:** Edge property indexes provide equality/range candidate postings but do not
  enforce uniqueness. Per-graph index clusters exist, while subject/range split axes across multiple
  index canisters remain planned.
- **Expected or needed behavior:** If uniqueness is exposed in DDL, the owning mutation boundary
  must reserve/check the key atomically with the canonical write. If an index is split, Router
  routing must preserve graph, subject, label, and range completeness without changing posting
  ordering semantics.
- **Evidence:** `design/index/property-index.md` Phase E and Non-goals; [ADR 0010](adr/0010-index-sharding-extensibility.md);
  current edge posting APIs in `crates/graph-index/src/facade/store/edge_postings.rs`.
- **Impact:** Declaring these capabilities prematurely would either allow duplicate values or make
  range/equality results incomplete across index canisters.
- **Next decision:** Keep both capabilities out of the public contract until their owner boundary,
  atomicity, routing, and rebuild semantics receive separate design decisions.

### GAP-2026-08-01-001 — gleaph-gql test harness: default-feature integration build broken, all-features suite hangs, clippy test lint fails

- **Status:** Resolved 2026-08-01 (commit pending; fixes in the same patch as the resolution)
- **Severity:** P3 test-harness
- **Owner:** `gleaph-gql` crate test harness
- **Observed behavior:**
  1. `cargo test -p gleaph-gql` (default features) fails to compile the `tests/parser_tests.rs`
     integration binary: `create_graph_type_nested_record_inline_property_ast` reads
     `PropertyDef.inline`, a `#[cfg(feature = "gleaph")]` field, without gating the test
     (parser_tests.rs L1378-1402).
  2. `cargo test -p gleaph-gql --all-features` does not terminate within the observation budget
     (10 minutes); `cargo test -p gleaph-gql --features cypher --lib` alone reproduces the hang,
     so the `cypher` feature's tests are the trigger.
  3. `cargo clippy -p gleaph-gql --all-targets --all-features -- -D warnings` fails on
     `clippy::items_after_test_module` in `type_check/phase_b.rs` (a `mod parameter_inference_tests`
     placed mid-file, followed by `pub fn infer_linear_query_binding_kinds`).
- **Root cause of the hang (2):** the deeply nested `VALUE { RETURN ...` input recursed through
  `Parser::recurse` until `MAX_RECURSION_DEPTH` (64), but the `cypher` build's enlarged
  expression/parse frames exhausted the native stack first — `EXC_BAD_ACCESS` (stack overflow) on
  the test worker thread, which the harness then waited on forever. Root cause confirmed by
  lowering the limit (guard fires, test passes) and by lldb (`EXC_BAD_ACCESS` at a stack address).
- **Resolution:**
  1. Gate `create_graph_type_nested_record_inline_property_ast` behind
     `#[cfg(feature = "gleaph")]` (the sibling `create_graph_type_edge_inline_property_ast` was
     already gated).
  2. Lower `Parser::MAX_RECURSION_DEPTH` from 64 to 32 with a doc note: the guard must fire at
     roughly half the heaviest feature build's stack cost; legitimate queries nest far below the
     bound. `cargo test -p gleaph-gql --all-features` now completes (all binaries pass, including
     the cypher-gated 746-test lib suite).
  3. Move `mod parameter_inference_tests` to the end of `type_check/phase_b.rs`;
     `clippy --all-targets --all-features -- -D warnings` now passes.
- **Evidence:** the three commands above before and after the fixes; `crates/gql/tests/parser_tests.rs`
  L1378-1402; `crates/gql/src/type_check/phase_b.rs`; `crates/gql/src/parser/helpers.rs`
  `MAX_RECURSION_DEPTH`; lldb backtrace showing `EXC_BAD_ACCESS (code=2)` at a stack address.
- **Impact (resolved):** default-feature test builds compile; the all-features suite terminates;
  test-target clippy gates with `-D warnings`.
- **Regression coverage:** the existing `deeply_nested_subqueries_are_rejected_not_overflowing`,
  `deeply_nested_parentheses_are_rejected_not_overflowing`, and
  `deeply_nested_not_chain_is_rejected_not_overflowing` tests run under both default and
  all-features builds and assert the depth error path.

## Review cadence

- The primary agent checks this ledger before final approval of a meaningful slice.
- A slice that resolves an entry updates its status in the same commit as the fix.
- Open entries should be converted to an implementation plan when their prerequisite arrives or
  when they become the highest-impact blocker.
- If an entry duplicates an existing roadmap or ADR item, replace its detailed proposal with a link
  to that authoritative contract rather than maintaining both descriptions.
