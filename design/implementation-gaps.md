# Discovered Implementation Gaps

Last updated: 2026-07-11
Anchor timestamp: 2026-07-11 07:55:50 UTC +0000

## Status

**Active tracking document** — this ledger records implementation defects, missing product
capabilities, and contract mismatches discovered while implementing another slice when they cannot
be resolved safely in that slice.

It is not a second roadmap or design source of truth. Each entry names the owning module and links
the active design contract. Once an architectural decision is accepted, the owning design document
or ADR remains authoritative and this ledger points to it.

## Disposition rule

Every material gap discovered during implementation, review, validation, or demo integration must
receive one disposition before the current work is committed:

1. **Fix now** when it is a correctness or security defect, blocks the current contract, has a clear
   owner, and can be repaired without obscuring the current slice.
2. **Prerequisite slice** when it blocks the current work but needs independent implementation,
   review, validation, or commit history.
3. **Record here** when it is real but non-blocking, its design is unresolved, or fixing it would
   expand the current slice materially.
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
  `crates/router/src/prepared.rs::resolve_prepared_graph_id`, and Plan 0044's
  `install_single_shard_federation_with_graph_admins` fixture.
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

## Resolved gaps

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
- **Related contracts:** [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md),
  [design/index/vector-index.md](../index/vector-index.md),
  [design/execution/pipeline.md](../execution/pipeline.md)

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

## Review cadence

- The primary agent checks this ledger before final approval of a meaningful slice.
- A slice that resolves an entry updates its status in the same commit as the fix.
- Open entries should be converted to an implementation plan when their prerequisite arrives or
  when they become the highest-impact blocker.
- If an entry duplicates an existing roadmap or ADR item, replace its detailed proposal with a link
  to that authoritative contract rather than maintaining both descriptions.
