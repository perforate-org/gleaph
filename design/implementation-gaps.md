# Discovered Implementation Gaps

Last updated: 2026-08-23
Anchor timestamp: 2026-08-23 13:11:17 UTC +0000

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

### GAP-2026-08-23-004 — Router canister wasm exceeds the IC code-section limit once the ADR 0074 slice 2a grant grammar is linked

- **Status:** Closed (2026-08-23) via prerequisite plan `0288` (router wasm budget recovery).
  Decision: build-path unification plus release-profile levers instead of source-level
  slimming. Every Gleaph canister build path (root `icp.yaml` script adapters,
  `scripts/deploy-demo-local.sh`'s instrumented router, the PocketIC fixture build,
  `scripts/check-codegen-local-e2e.sh`) now runs one shared post-processing entry point,
  `scripts/postprocess-canister-wasm.sh` (`ic-wasm` metadata insertion → candid extraction →
  `ic-wasm shrink --keep-name-section`; the name section stays kept deliberately — it is a
  custom section and does not count against the code-section limit), and `[profile.release]`
  gained `lto = "fat"` + `codegen-units = 1`. Binaryen `wasm-opt` was not needed.
- **Observed behavior:** `cargo test -p gleaph-pocket-ic-tests` fails every suite with
  `Wasm module code section size of 12615453 exceeds the maximum allowed size of 12582912`
  during Router install (PocketIC enforces the mainnet limit). Measured code sections of
  `gleaph_router.wasm` (wasm64, release, E2E features): HEAD + concurrent-session overlay
  without slice 2a = **11,942,769**; with slice 2a = **12,123,923** (+181,154); six-package
  feature-unified E2E build = **12,615,455** (+32,543 over the limit).
- **Attribution evidence:** name-section diff of the two single-package binaries shows the
  largest genuinely new linkage is `data_encoding` decode machinery (**+61,772 bytes**, 19 → 49
  functions), introduced by the first production use of `candid::Principal::from_text` in
  `crates/router/src/gql_grants.rs::bind_subject` (all pre-existing `from_text` call sites are
  test-only; `to_text` was already linked via provisioning `deployment_id`). An isolated
  experiment replacing `from_text` with `from_slice` shrinks the binary by 76,068 bytes,
  confirming the attribution; the remaining ~105KB of delta is inlining-budget churn spread
  across unrelated shared symbols (growth and shrinkage nearly cancel: +1.87 MB / −1.87 MB).
- **Needed behavior:** ADR 0074 §5 requires the integration layer to bind `PRINCIPAL <literal>`
  subjects to identities, which requires principal-text decoding in the Router; the IC
  install-time code-section ceiling is hard on both mainnet and PocketIC.
- **Owner:** Router canister build composition (`crates/router`, workspace release/wasm64
  profile, `crates/pocket-ic-tests/build.rs`). The principal-text contract itself stays owned
  by `candid`; a hand-rolled base32/CRC decoder in the Router was considered and rejected as a
  duplication of candid's identity-validation logic. Resolution adds
  `scripts/postprocess-canister-wasm.sh` and the workspace `[profile.release]` as the owning
  size-budget surfaces.
- **Impact:** ~~all PocketIC E2E suites are blocked until the Router drops ≥33 KB below the
  current footprint~~ resolved: see resolution evidence below; plan `0287` todo
  `pocketic-lifecycle-tests` is unblocked.
- **Resolution evidence (2026-08-23, plan `0288`; limit 12,582,912 code-section bytes,
  measured with `scripts/wasm-code-section-size.py`):**
  - PocketIC fixture path (six-package feature-unified wasm64+SIMD release), isolated levers:
    raw cargo output **12,123,923** → + `[profile.release] lto="fat", codegen-units=1`
    **10,689,428** (−1,434,495) → + shared post-processing **9,634,923** (−1,054,505).
    Headroom vs the limit: **2,947,989 bytes (~2.81 MiB)**, ~11× the required 256 KiB.
    Today's pre-change reproduction measures lower than the recorded failing build
    (nightly `c656540d6 2026-08-21` and/or concurrent-session overlay churn; not bisected —
    applying the same measured deltas to the recorded 12,615,455 footprint estimates ≈10.13 MB,
    still ≥2.4 MiB under).
  - Per-path router code sections after both levers: deploy (`icp build gleaph-router`)
    **9,597,708**; demo instrumented (`batch-instr-log`, feature-guard grep verified)
    **9,614,992**; PocketIC fixture **9,634,923** — all paths installable with ≥2.8 MiB margin.
  - Other canisters strictly smaller than before on every path (PocketIC fixture code
    sections, before → after): graph 7,405,381→6,094,339; graph-index 2,098,383→1,744,550;
    vector 1,890,704→1,512,047; account 973,145→805,582; provision 1,677,761→1,382,074.
  - Blocked validations rerun green: `smoke` 1/1, `adr0074_auth_ingress_walk` 1/1,
    `adr0074_grant_grammar` 4/4 (first-ever execution exposed a self-contradictory hardcoded
    subject expectation in the suite's own `expect_grant_summary` helper — it asserted `Public`
    while its callers grant to `PRINCIPAL` and assert the principal subject; the helper now
    takes the expected subject, no canister-side change was needed, stored rows were correct).
- **Next decision:** none (closed). Standing watch: cross-nightly rebuild drift is real
  (~±500 KB observed); keep the ≥256 KiB headroom check per canister when touching canister
  features or toolchain versions, measured with `scripts/wasm-code-section-size.py`.

### GAP-2026-08-23-003 — ForceAtlas2 equilibrium rests below the rendered node diameter once nodes are world-sized

- **Status:** Closed (2026-08-23). Decision: recalibrate the repulsion-scaling
  default so the force balance itself rests outside marker overlap
  (`scaling = 432`, derived from `d* = (S·m²)^{1/3}` with mass 2 pairs equaling
  the canonical 12-unit node diameter), documented in DESIGN.md §15.1. A hard
  pairwise separation floor was prototyped first and rejected: dense topologies
  make it unsatisfiable (hub/256 leaf ring spacing ~1.2 units against a 12-unit
  floor), so the constraint fights the force law forever and the layout never
  settles. Residual limitation, accepted as capacity physics rather than a
  defect: leaves around a massive hub still rest partially overlapped
  (~9 units apart at hub/256) because their packing density exceeds what any
  spacing floor allows — matching reference FA2 behavior. If overview-zoom
  readability of such dense hubs ever matters, revisit render-side marker LOD
  as a separate slice.
- **Resolution evidence:** undilated headless probe over the example and
  tolerance fixtures (24-node demo, 12-node team start, hub/256 ring, grid/20x20@40):
  average relaxed edge length moved from 4.8 / 3.9 / 10.6 / 9.0 world units
  (default scaling 1) to 35.8 / 29.7 / 73.8 / 33.3 (scaling 432) — every family
  at or beyond ~2.5× the rendered diameter except capacity-limited hub leaves;
  settle iterations stayed far inside the contract budget (156 / 109 / 128 /
  471 against < 1500); `cargo test -p gpui-graph --lib` 217 passed including
  `settles_within_iteration_budget` and `slow_down_scales_per_iteration_motion`.
- **Follow-up (2026-08-23, same day):** the canonical `node_radius` default was
  halved to 3 (diameter 6 world units); by the same pair-equilibrium derivation
  the FA2 scaling default moved 432 → 54 (`d* = (54·2²)^(1/3)` = 6). Fixture
  ratios were preserved (demo/24 ratio 3.01 / team 2.50 / hub 6.45 / grid 3.47;
  settle iterations 204 / 66 / 195 / 528), confirming the calibration is
  scale-free. DESIGN.md §15.1 records the current numbers.
- **Severity:** P2 visual quality — demo-scale graphs relax into overlapping node blobs over
  tens of seconds; static and freshly-opened views are unaffected.
- **Owner:** `crates/gpui-graph/src/layout/force_atlas2.rs` force balance, against the
  world-sized node contract (`3a52ee395`, DESIGN.md §26.2) and the default placement contract
  (DESIGN.md §13).
- **Observed behavior:** Headless probe mirroring the `force_atlas2` / `interactive` examples
  (24-node random hub graph, 12-node start graph, FA2 defaults, one iteration per frame): from
  the ±200-unit spread start, the average edge length falls from ~205–233 world units at frame
  0 to **4.7–6.5 by settle** (frame ~1400 undilated pace) — i.e. below half the rendered node
  diameter (`2 * node_radius` = 12 world units), with a minimum pairwise node distance of
  ~2.3 units. Before world-sized nodes this contraction was invisible (nodes drew at a fixed
  6 px); now the settled state renders as heavily overlapping discs.
- **Mechanism:** linear attraction grows with distance while repulsion falls off as
  `1/d²`, so the pairwise equilibrium sits near `scaling^(1/3)` times small mass factors —
  well inside one node diameter at the default `scaling = 1`. A universal hard separation
  floor was prototyped and rejected: dense topologies make the floor unsatisfiable (hub/256
  leaves around their center equilibrate at ring spacing ~1.2 units; enforcing ≥12 units per
  pair puts the constraint in permanent conflict with the force law, so the layout never
  settles — `layout::force_atlas2::tests::settles_within_iteration_budget` fails at budget),
  and raising attraction-side softening or `scaling` alone either misses the floor or breaks
  convergence contracts elsewhere.
- **Impact:** relaxed layouts eventually render overlapping markers in every app using FA2
  defaults with the canonical style; hit-testing ambiguity follows visually.
- **Next decision:** pick one contract before coding: (a) a size-aware FA2 parameter with a
  capacity-aware relaxation that tolerates density where the floor cannot hold (Gephi-style
  no-overlap is approximate for exactly this reason); (b) render-side marker LOD that shrinks
  or fades markers when local density exceeds what world-sized discs allow; or (c) document
  per-app tuning of `with_scaling`/`node_radius` pairs and accept blobbing for dense hubs.
  Record the decision in DESIGN.md §11/§26.2 and close this entry in the fixing commit.

### GAP-2026-08-22-001 — Migration-driven directed edge builds reject at graph-index seeding: wire vs catalog label identity divergence

- **Status:** Closed (2026-08-23; identity rule decided and implemented per the Next decision
  below — plan 0282). Owning evidence: graph-index unit contract green
  (`cargo test -p gleaph-graph-index --lib` 153 passed / 0 failed, including
  `directed_wire_fact_seeds_under_catalog_target_and_catalog_sieve_finds_it`,
  `any_direction_target_accepts_both_bucket_packings_of_its_catalog_label`,
  `facts_from_a_different_catalog_label_are_rejected_and_store_nothing`, and
  `dml_build_subjects_carry_wire_labels_against_catalog_targets`); cross-canister closing proof
  green (`cargo test -p gleaph-pocket-ic-tests --test adr0059_index_build_lifecycle`
  **5 passed / 0 failed** with both previously-ignored inline scenarios un-ignored and passing:
  `edge_inline_create_index_migration_converges_active_with_complete_postings` converges a
  directed ROAD + undirected LINK migration over pre-existing two-shard inline values with
  posting-level shard/canonical-owner assertions, and
  `edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges` preserves watermarks across a
  same-wasm mid-build upgrade of all five federation canisters); regression guards green
  (`router_gql_query` 24 passed / 0 failed including every standalone edge lifecycle target).
- **Severity:** P0 index correctness — blocks the edge half of ADR 0059's lifecycle and therefore
  the GAP-2026-07-29-001 E2E closure
- **Owner:** The Router→Graph→graph-index edge build identity contract: Router registration
  (`crates/router/src/index_catalog.rs::resolve_index_definition`,
  `facade/store/schema_migration/index.rs::step_request`), Graph export fact emission
  (`crates/graph/src/index/canonical_export.rs::export_edge_sidecar_page` /
  `export_edge_inline_page`), and graph-index identity enforcement
  (`crates/graph-index/src/facade/store/build_state.rs::prepare_fact_posting`)
- **Observed behavior:** Both new PocketIC scenarios in
  `crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`
  (`edge_inline_create_index_migration_converges_active_with_complete_postings`,
  `edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges`, currently `#[ignore]`d) drive a
  combined `CREATE INDEX ... FOR ()-[e:ROAD]-() ON (e.distance)` + undirected LINK migration
  through `apply_schema_migration` over a two-shard federation with pre-existing inline edge
  values. The first (directed ROAD) sub-build registers and reaches Building, then terminates with
  `SchemaMigrationApplyStatus::Failed(TargetRejected)`; in the upgrade variant
  `IndexBuildProgress.seeded_items` stays 0 until the round budget exhausts. Vertex builds are
  unaffected (their facts carry no label).
- **Mechanism (static chain, each link read directly on main `306f469c1`):**
  1. The Router registers the build target with the untagged catalog label id:
     `resolve_index_definition` uses `lookup_edge_label_id(...).raw()` and
     `IndexDefRecord.label_id` persists it; `step_request` copies it into
     `IndexBuildTarget::Edge { label_id }`.
  2. Graph storage keys edges by tagged wire labels (`BUCKET_LABEL_DIRECTED_BIT = 0x8000`;
     directed bucket key = catalog id | 0x8000, undirected = catalog id). Both edge export pages
     emit facts whose `label_id` is that storage value (`key.label_id()` / `edge.label_id()`).
  3. graph-index requires exact equality between fact label and registered target label
     (`target_label == label_id`) and returns `InvalidIndexBuildTarget` otherwise.
  4. `InvalidIndexBuildTarget` is not retryable, so the driver classifies it as
     `MigrationFailureCode::TargetRejected`.
  For a DIRECTED edge, wire (`id | 0x8000`) never equals the registered catalog id, so every seed
  page rejects.
- **Expected or needed behavior:** One owner must define the posting label identity end-to-end so
  migration-seeded postings, DML-maintained postings, and read-side binding agree for both
  directedness buckets. Note the same divergence family exists on the Active DML/read path today:
  DML maintenance inserts postings under wire labels
  (`crates/graph/src/index/edge_pending.rs::push_edge_index_op` receives the canonical wire label)
  while expand lookups sieve with resolved catalog ids
  (`plan/query/executor/expand/candidates.rs`), so directed-edge equality probes through
  graph-index return zero hits and silently degrade to scan fallbacks
  (`expand_candidates_via_equality_index` treats empty postings as "index did not own the lookup").
  Single-shard lifecycle tests pass because of that fallback, masking the divergence.
- **Evidence:** Scenario run 2026-08-22: inventory precondition `router_gql_query` 24 passed /
  0 failed; `adr0059_index_build_lifecycle` 3 passed / 2 failed with
  `Failed(TargetRejected)` and a stuck-at-zero `seeded_items`. Unit coverage gap: graph-index's
  build-state tests use one arbitrary label id (9) for both target and facts, so the divergence
  has no unit-level signal.
- **Impact:** No index over a directed edge property can be created through the migration
  lifecycle (terminal failure); GAP-2026-07-29-001 cannot close. Undirected-only registrations
  happen to satisfy the numeric identity check but then bind read handles from catalog-id
  postings, so fixing only the register side without owning the identity decision would leave
  read binding broken for one bucket packing.
- **Next decision:** Decided (2026-08-22, plan 0282 survey): **edge postings are stored and
  transported in wire-tagged space (`catalog id | directed MSB` directed, bare catalog id
  undirected); registrations, memberships, and lookup sieves speak catalog ids.** Rationale:
  every physical producer already emits wire labels (DML dispatch from canonical handles,
  repair journal, operator backfill export, migration seed facts) and read binding resolves
  `EdgeHandle`s from posting labels against LARA bucket keys, so wire storage needs zero
  producer churn and keeps handle binding unambiguous; the catalog-space alternative would
  force a translation onto every Graph producer and make handle binding direction-ambiguous.
  The single translation owner is `gleaph_graph_kernel::entry::label` — `EdgeLabelId`
  (catalog) ↔ `TaggedEdgeLabelId` (wire) via `pack()` / `label_index()`; no ad-hoc `0x8000`
  arithmetic outside it. Conforming seams: (S1) graph-index `prepare_fact_posting` accepts a
  fact when its wire label's catalog index equals the registered catalog label and its bucket
  is covered by the registration direction, storing under the wire tag (`EdgePostingKey`
  layout and Candid types unchanged); (S2) `ensure_subject_matches_target` applies the same
  rule to DML build subjects; (S3) graph-index `lookup_edge_{equal,range}_page` treat
  request labels as catalog ids and scan both packings; (S4) Graph's
  `lookup_edge_{equal,range}_local` resolve memberships by direct catalog equality instead of
  feeding a catalog id into the wire-expecting registration matcher. Router registration,
  export scopes/walks, planner stats, and all Graph producers stay unchanged.

### GAP-2026-08-21-002 — Graph registration completion held bootstrap intent locks across a deployment's graph bootstraps

- **Status:** Resolved 2026-08-22 by `3ef8fe9d53f503b9663e1e5fe324741a16005559`
- **Severity:** P2 provisioning lifecycle
- **Owner:** Router Graph provisioning convergence and Provision's versionless `complete_graph_registration` endpoint
- **Historical observed behavior (before the 2026-08-22 working-tree fix):** After a successful `CREATE GRAPH` bootstrap, the Router-side request record stayed `AwaitingAck` and its `(deployment_id, GraphShard(0))` intent lock stayed held because no Provision→Router ack arrived. A second `CREATE GRAPH` by the same caller principal failed with `Conflict("provisioning intent already locked")` indefinitely (`adr0070_create_graph_provisioning.rs` first exercised this as an infinite retry).
- **Expected or needed behavior:** Once Router has reconciled the exact Graph topology, its versionless completion call must mark the Provision job `Completed` and release only that request's Map 2/3 and Map 47 rows, so sequential bootstraps converge.
- **Evidence:** Fixing commit `3ef8fe9d53f503b9663e1e5fe324741a16005559`. Exact Router owner
  tests cover retry-before-early-return, partial registration, lost-response replay, byte-exact reopen,
  and owned Map 47 release. The Provision owner lifecycle test drives the production accept and
  completion handlers across an actual Map 1/2/3 reopen, preserves exact created resources, effect
  count, and immutable-envelope digest on admission replay, and proves completed replay leaves a later
  request's Map 2/3 rows unchanged. On 2026-08-22 UTC, both focused PocketIC runtime targets passed:
  `create_graph_provisions_shard_and_sets_home_graph` proved same-caller consecutive graphs, and
  `router_graph_bootstrap_registration_ack_crosses_real_candid_boundary` proved the real
  Router→Provision Candid completion boundary (1 passed, 0 failed for each target).
- **Impact:** The exact `GraphShard(0)` path no longer has a one-graph-per-deployment restriction.
  Property- and Vector-index completion remain deferred to their owner-specific slices.

### GAP-2026-08-21-001 — Anchored multi-DML roll-forward saga fails to converge on both shards

- **Status:** Resolved 2026-08-22 (fix implemented on top of `7a75e9fd6` and validated there; commit pending at time of update)
- **Severity:** P1 DML correctness signal
- **Owner:** Graph vertex-index membership resolution (`crates/graph/src/index/catalog_context.rs::vertex_index_memberships_for_labels`), shared by single DML, bulk insert, and backfill; E2E fixture catalog guards.
- **Observed behavior:** In
  `crates/pocket-ic-tests/tests/router_gql_query.rs`, three consolidated contracts failed:
  `router_runs_anchored_multi_dml_bundle_across_shards_as_roll_forward_saga`,
  `router_recovers_anchored_multi_dml_roll_forward_saga_via_idempotent_retry`,
  `router_recovers_non_terminal_federated_saga_via_idempotent_retry`. The two-shard bundle reports
  `Completed`, but the post-commit read `MATCH (n {age: 6}) RETURN n` returned 0 rows instead of the
  expected 2, so the SET did not survive on either shard despite the completed phase.
- **Mechanism (probe-verified on `7a75e9fd6`):** Nothing removed the old posting. Fixture inserts
  posted through `e2e_legacy_unlabeled_vertex_catalog_guard`, which rewrote every vertex
  membership's `label_id` to 0 so unlabeled vertices matched the label-scoped index; bundle-time
  SET resolved memberships against the true Router catalog, where the same unlabeled vertex
  (`labels == []`) matched nothing, so neither remove-old nor add-new was ever enqueued
  (`pending_min == None` because the queue was never populated). Post-bundle reads still seeded
  from the stale posting (`lookup_equal` hits=2) but the shard-side residual equality filter
  dropped the rows because canonical age had changed. The labeled variant failed admission for a
  second reason: its fixture posted into a fabricated physical namespace (101) instead of the
  Router-allocated namespace (1), so anchor lookups saw no hits.
- **Expected or needed behavior:** A `Completed` roll-forward saga must leave both shards'
  anchor vertices updated; the read-back equality anchor must observe both.
- **Resolution:** `vertex_index_memberships_for_labels` now owns the maintenance rule in one
  place: a vertex with no labels maintains every namespace indexing the property (legacy-unlabeled
  contract, mirroring the Router's label-less superset anchor reads); labeled vertices maintain
  wildcard plus exact-label memberships as before; a missing vertex row dispatches nothing. The
  fixture-only wildcard rewrite and fabricated namespaces were deleted — fixtures fetch the real
  Router catalog via `e2e_router_catalog_guard`. Regression origin diff-triaged to `04575f280`
  (strict label-scoped DML membership resolution), not to the five commits named in plan 0269
  Step 2; no bisect was needed.
- **Owning regression tests:** graph-unit `unlabeled_legacy_set_reposts_remove_and_insert_into_label_scoped_membership`,
  `labeled_set_reposts_only_into_its_own_label_membership`,
  `remove_repost_removes_exactly_the_old_value_posting`,
  `multi_label_vertex_sharing_one_namespace_pushes_no_duplicate_ops`,
  `missing_vertex_row_dispatches_no_postings` (`catalog_context.rs` tests); PocketIC
  `router_labeled_insert_set_repost_converges_across_shards` plus the restored saga trio in
  `router_gql_query.rs` (20 passed / 0 failed at the fix).
- **Evidence:** Probe instrumentation over a pinned worktree of `7a75e9fd6` (no working-tree
  changes): fixture-phase `resolved=[(1,1)]` + `Insert` posting vs bundle-phase `resolved=[]`
  twice per shard; zero index-canister vertex-posting mutations during the bundle; post-bundle
  seed hits=2 sieved to 0 rows by the residual filter; labeled variant pre-bundle eq(5)=0 with
  `ix batch phys=101`.

### GAP-2026-08-20-001 — AtLeast graph-index barrier ignores pending first-delivery outbox work

- **Status:** Resolved (commit pending; fix is in this patch)
- **Severity:** P0 read consistency
- **Owner:** Router ReadMode::AtLeast barrier and the Graph-owned durable derived-index work boundary
- **Observed behavior (before fix):** GraphStore::index_pending_min_mutation_id read only the
  failure-only RepairJournal, while DerivedIndexOutbox was a separate durable FIFO whose entries
  retain their originating mutation_id. Router's AtLeast barrier therefore could consult only the
  repair owner.
- **Expected or needed behavior:** An index-backed AtLeast(M) read must fail closed until every
  derived posting at or below M has reached its target, including first-delivery and failed-flush
  work.
- **Resolution:** Graph MemoryId 52 now owns one fixed key for every qualifying source row in
  DerivedIndexOutbox and RepairJournal. GraphStore prevalidates and synchronously co-updates the
  source row and its exact floor key; zero-id and `IndexBuildDml` rows are excluded, quarantine
  preserves its key, and acknowledgements remove only exact applied rows. Router's existing barrier
  and wire shape are unchanged.
- **Evidence:** `fixed_keys_order_by_mutation_owner_and_sequence`,
  `fresh_and_reopen_preserve_exact_floor`,
  `co_updates_outbox_and_repair_without_zero_or_build_rows`,
  `quarantine_and_partial_ack_preserve_exact_multiplicity`,
  `prevalidation_failure_leaves_owners_and_floor_unchanged`, and PocketIC
  `first_delivery_outbox_survives_graph_upgrade_and_atleast_fails_closed_until_drain`.
  ADR 0029 §5 remains the AtLeast projection-barrier authority.
- **Validation status:** The five exact Graph owner tests, layout/stats tests, focused floor and
  reopen canbenches, Graph check/clippy, and stopped-index PocketIC lifecycle pass. Final unfiltered
  Graph benchmark persistence and final plan/scope gates are recorded in Plan 0263.
- **Impact:** A token can be treated as graph-index satisfied while its first delivery remains
  durable but unapplied, allowing an index-backed read to miss canonical state.
- **Next decision:** Keep the separate Vector Index acknowledgement/watermark/tombstone-retention contract and
  index-build generation visibility contract in their later slices; do not infer either from this
  ordinary Graph-owned floor.

### GAP-2026-08-20-002 — Router direct vector ingestion durable intent ownership

- **Status:** Resolved (2026-08-22; catalog-driven markerless frontier implementation and focused
  runtime/benchmark gates pass; the persisted Router artifact has all 41 terminal benchmark entries,
  including `markerless_frontier_catalog` total 52,080,138 and scope 52,079,143 instructions;
  targeted affected-crate format passes while the workspace-wide format check is blocked by unrelated
  dirty paths and never passed; finite-time liveness, global subject-map growth, and finite-time GC
  remain explicitly deferred)
- **Severity:** P0 durable derived-state contract
- **Owner:** Router direct vector-ingestion API and MemoryId 53 lifecycle
- **Contract:** Every allocated direct-ingestion mutation ID is synchronously co-written with one
  exact durable intent before the first Graph await. `Pending` is valid only while that intent is
  `AwaitingGraph`, `AwaitingVector`, or `AwaitingFrontier` in the sole MemoryId 53 lifecycle. The
  Router row durably owns pending/retry payload bytes until exact marker retirement; Vector owns the
  indexed embedding bytes after delivery.
- **Implementation:** `admit_awaiting_graph` validates the full batch, live shard, exact Graph and
  Vector targets, immutable definition, capacity, and encoded size before changing state. It then
  co-writes the final `ROUTER_MUTATION_COUNTER` value and every canonical `AwaitingGraph` row with no
  intervening await. Exact Graph acceptance changes only the matching row to `AwaitingVector`;
  exact logical rejection changes only that row to `AwaitingFrontier`; transport/decode failure and
  response loss retain the current phase. Vector typed-prefix acknowledgement changes only applied
  `AwaitingVector` rows to `AwaitingFrontier`. The canonical shard catalog enumerates only fully
  attached, non-anonymous `(Vector target, shard)` lanes. For the selected lane, Router derives the
  frontier from the oldest unresolved exact-lane intent minus one, or the durable allocation ceiling
  when none remains, and may publish an empty marker snapshot. Unresolved direct intents in another
  lane do not cross-block. The recovery cursor advances before await and attempts one lane per tick,
  so a failed lane yields later lanes a turn before retry after lap rotation. The Router-only Vector
  endpoint rechecks Router ownership and exact shard attachment, applies MemoryId 15 with `max`, and
  runs one bounded GC step in the same no-await update; its cutoff remains
  `min(graph_watermark, router_watermark)`. Recovery uses persisted targets for direct-ingestion rows
  and catalog discovery only for markerless lane enumeration.
  MemoryId 2 now stores the public shard projection and a Router-private monotonic Vector attach
  epoch in one canonical row. Unregister advances the epoch while clearing both readiness bits
  before the first detach await and returns the exact retained Vector target plus epoch as its
  continuation claim. Router revalidates that claim and both cleared bits before each outbound
  detach and atomically before final removal. Successful existing-row index re-registration
  advances the epoch once while clearing the old target; an already completed duplicate is a no-op.
  A new explicit same-target attach advances the epoch again, so every pre-unregister finalizer and
  unregister continuation exact-conflicts and only the new claim may publish readiness or retain
  the row.
- **Evidence:** `crates/router/src/facade/stable/vector_ingest_outbox.rs::{admit_awaiting_graph,
  observe_graph_accept,observe_graph_reject,apply_outcome,run_recovery_pass}`,
  `crates/router/src/facade/stable/graph_catalog.rs::scan_attached_vector_lane`, and
  `crates/router/src/api/control.rs::ingest_vertex_embeddings`. Owner-local tests cover admission
  atomicity, phase persistence, exact compare-before-transition, stale callbacks, reopen, Vector
  prefix retention, empty/non-empty frontier snapshots, catalog enumeration, one-lane rotation,
  malformed outcomes, and recovery scheduling. The focused PocketIC test
  `graph_only_markerless_lane_advances_frontier_on_autonomous_timer` passes exactly one test with
  seven filtered tests, one `PocketIc`, one federation bootstrap, and four canister installs (Router,
  Property Index, Graph, Vector); it covers ordinary timer dispatch, applied response loss, upgrade
  rediscovery, exact retry, and physical collection under the Vector
  `min(graph_watermark, router_watermark)` cutoff. The persisted Router
  `crates/router/canbench_results.yml` artifact has all 41 terminal benchmark entries;
  `markerless_frontier_catalog` records exactly 52,080,138 total instructions and 52,079,143
  instructions in its benchmark scope. The dated Plan 0277 focused/persisted Vector
  `bench_router_frontier_gc_budget` baseline records 2,110,300,092 total and 2,110,299,087 scoped
  instructions; the later live artifact currently records 2,084,327,695 total and 2,084,326,690
  scoped instructions from unrelated work and is not this slice's evidence. Nine exact Router
  owner unit tests pass:
  `stale_vector_attach_finalizer_after_unregister_reregister_requires_new_attach`,
  `same_target_vector_reattach_claim_fences_delayed_pre_unregister_finalizer`,
  `unregister_start_response_loss_then_register_requires_explicit_vector_reattach`,
  `second_graph_same_vector_target_and_shard_cannot_duplicate_lane_ownership`,
  `markerless_failure_rotates_to_next_lane_and_wraps_without_starvation`,
  `vector_attach_rejects_cross_page_duplicate_before_claiming_candidate`,
  `unregister_rejects_exact_pending_vector_work_without_lifecycle_mutation`,
  `unregister_shard_with_vector_target_removes_vector_readiness_row`, and
  `delayed_unregister_final_commit_cannot_remove_new_same_target_ownership_after_vector_detach`.
  The separate regression
  `delayed_unregister_cannot_detach_or_remove_new_same_target_ownership` also passes and protects
  the post-await unregister claim fence. Router check passes. Router all-target/all-feature clippy
  is blocked only by unrelated unused imports and one needless borrow
  in `crates/router/src/prepared.rs`; production-library strict clippy passes with no markerless
  diagnostic. The targeted affected-crate format check passes
  while the workspace-wide format check is blocked by unrelated dirty paths and never passed. The
  Quint refinement passes 15/15 deterministic tests and both 5,000-sample depth-35 safe-policy runs
  with all requested witnesses; `quint verify` remains unrun and non-required. This bounded model
  and the focused runtime evidence are not production proof of finite-time liveness or global
  subject-map growth.
- **Impact:** Restart or response loss cannot leave an allocated direct-ingestion stamp without a
  durable owner. Fully attached catalog lanes, including Graph-only lanes with no MemoryId 53 row,
  can now advance the Router frontier and use the existing
  `min(graph_watermark, router_watermark)` cutoff. The public operation remains at-least-once
  retry/convergence with no finite-time or client-level exactly-once guarantee, and no global
  subject-map growth or finite-time GC bound is claimed.
- **Next decision:** No remaining prerequisite exists for this bounded durable-ownership and
  markerless eligibility slice. Keep finite-time liveness, global subject-map growth, and finite-time
  GC as separate future evidence; do not infer those claims from the bounded Quint model or the
  one-lane runtime gate.

### GAP-2026-08-20-003 — CanonicalPending retry does not reconcile a completed Graph receipt

- **Status:** Resolved in `d700331c33a8cdae7524e76bcb4e9ddcd5cdb600`
- **Severity:** P1 Router exact-replay recovery
- **Owner:** Router ordered atomic_insert lifecycle and exact Graph journal reconciliation
- **Observed behavior (before fix):** ADR 0049 requires a same-key retry after a Graph success /
  Router callback loss to query the exact Graph journal and either record its completed receipt or
  resend the exact stored request. Ordered edge, vertex, and mixed recovery left CanonicalPending
  with an explicit-retry diagnostic, and an existing same-key request returned its Router record
  without reconciliation.
- **Expected or needed behavior:** If the exact Graph journal contains a completed receipt, retry
  must advance the Router record without canonical redispatch. Only an explicit exact retry may
  redispatch an unambiguously absent stored target; background recovery must remain query-only and
  fail closed.
- **Resolution:** Existing ordered CanonicalPending admission now reaches Router's private
  trigger-aware reconciliation boundary before fresh routing or catalog resolution. It reads the
  persisted Graph target and accepts only exact active, completed journal evidence to record the
  matching receipt without an ordered execution. Explicit same-key retry may redispatch only after
  an unambiguous Absent result and only with that immutable stored request. Background recovery is
  query-only: it may adopt exact completed evidence, while Absent, invalid, retired,
  not-applicable, non-completed, and identity/version/fingerprint/count/allocation mismatches keep
  CanonicalPending with only the bounded diagnostic change and no ordered execution.
- **Evidence:** Router tests
  canonical_pending_reconciliation_uses_stored_target_before_fresh_resolution,
  canonical_pending_reconciliation_adopts_exact_completed_receipts_without_dispatch,
  canonical_pending_reconciliation_rejects_nonexact_evidence_without_dispatch, and
  canonical_pending_reconciliation_handles_absent_by_trigger; PocketIC tests
  atomic_insert_canonical_pending_reconciliation_same_key_retry_does_not_redispatch_completed_journal
  and
  atomic_insert_canonical_pending_reconciliation_timer_after_router_upgrade_does_not_redispatch_completed_journal.
  The final focused Router filter reported 4 passed / 792 filtered. The focused PocketIC filter
  reported 2 passed / 13 filtered after the test-only target_shard expectation correction. Router
  library clippy, Graph pocket-ic-e2e clippy, and the PocketIC target clippy passed.
- **Supplemental runtime evidence:** After the latest Router reservation fix, the full
  adr0057_atomic_insert target ran once and reported 14 passed / 1 failed. Both reconciliation
  tests passed. This target is not counted as a pass: the unrelated
  atomic_insert_rejects_missing_catalog_name_before_reservation test failed because PocketIC HTTP
  adapter startup timed out and reqwest reported IncompleteMessage.
- **Close-gate status:** Global `cargo fmt --all -- --check` passed. The final Plan 0266 validator
  passed. The final exact 7-path scope review passed with the staged-empty/unrelated manifest
  matched. Both scoped Plan tests passed. The full `adr0057_atomic_insert` target remains
  incomplete at 14 passed / 1 failed because the unrelated
  `atomic_insert_rejects_missing_catalog_name_before_reservation` test hit a PocketIC HTTP
  adapter startup timeout (`IncompleteMessage`). No benchmark was run because this slice does not
  change a performance-sensitive algorithm. The recovery fix was committed as
  `d700331c33a8cdae7524e76bcb4e9ddcd5cdb600`.
- **Impact (before fix):** A committed canonical effect could remain non-terminal despite the
  exact durable Graph receipt needed to reconcile it.
- **Next decision:** None for this recovery contract. Plan 0260 remains a later, non-blocking
  formalization slice.

### GAP-2026-08-20-004 — Rust application client SDK and shared Router wire contract

- **Status:** Resolved
- **Severity:** P2 product gap; generated-code quality gap for the Rust client profile
- **Owner:** `gleaph-codegen` rust client profile, `gleaph-cdk`, and the SDK boundary
- **Observed behavior:** The Rust application-client codegen profile (`generate_rust`) emitted a
  provisional `PreparedExecutor` / `PreparedQueries` facade whose transport and response decoding
  were a runtime scaffold, and it did not mirror the mature `PreparedExt` profile used by
  `gleaph-cdk`. There was no supported Rust application client; the Router data-plane wire contract
  lived only in `gleaph-cdk::types`, which depends on `ic-cdk` and is not a valid application-client
  dependency.
- **Resolution (ADR 0069, 2026-08-20):** introduced `gleaph-router-wire` (`crates/router-wire`) as a
  neutral owner of the Router data-plane wire contract (`GqlQueryResult`, `ReadMode`,
  `MutationToken`, `RouterError`, bulk-load family, row-decode helpers); `gleaph-cdk` re-exports it.
  Introduced `gleaph-sdk` (`sdk/client/rust`), an `ic-agent` application client mirroring the
  `GleaphClient<Prepared>` surface with a `GleaphTransport` trait, `IcAgentTransport`, and caller
  identity injection through the agent identity. Unified the Rust codegen profiles onto a shared
  renderer (`crates/codegen/src/rust/shared.rs`) parameterized by runtime path; the client profile
  now emits the same `PreparedExt` boundary as the canister profile, removing `PreparedExecutor` /
  `PreparedQueries` and the provisional client envelope/structs.
- **Evidence:** `crates/router-wire`, `sdk/client/rust`, `crates/codegen/src/rust/shared.rs`;
  updated fixture at `crates/codegen/fixtures/rust-client-basic/src/lib.rs`; codegen tests in
  `crates/codegen/src/lib.rs`.
- **Impact:** Resolved: application clients get a typed, canister-parity client with identity
  injection, and both Rust profiles share one generated `PreparedExt` shape and one wire contract.
- **Next decision:** None — ADR 0069 accepted; the generated Rust client output changed from the
  `PreparedExecutor` scaffold to `PreparedExt`, so any scaffold consumer must regenerate.

### GAP-2026-08-20-006 — Router shard identity has no incarnation or lifecycle fence

- **Status:** Resolved (2026-08-22)
- **Severity:** P0 identity safety
- **Owner:** Router graph catalog lifecycle, ShardId allocation, and Graph/Router wire identity
- **Resolution:** `ROUTER_GRAPH_RUNTIME_CONFIG.next_shard_id` is the canonical per-graph high-water.
  Registration accepts only that never-issued id and advances the high-water; unregister and
  failed-attach rollback never rewind it. Exhaustion fails before any registry mutation.
- **Evidence:** `graph_runtime_config_reopen_preserves_shard_high_water`,
  `unregister_shard_never_reuses_the_retired_graph_local_id`,
  `exhausted_graph_local_shard_allocator_rejects_without_registry_mutation`, and
  `registry_invariants_reject_active_shard_at_allocator_high_water`.
- **Impact:** Delayed work and public identifiers from a retired shard cannot alias a later shard
  within the same graph, without widening `GlobalVertexId` or cross-canister protocols.
- **Next decision:** None for shard identity. Frontier publication remains a separate Vector GC
  protocol decision.

### GAP-2026-08-20-005 — Property DROP INDEX has no durable per-PhysicalIndexId retirement lifecycle

- **Status:** Resolved (2026-08-22; fix is in this patch)
- **Severity:** P1 derived-index cleanup
- **Owner:** Router index catalog and graph-index posting purge lifecycle
- **Observed behavior:** Router removes the catalog row before remote purge and keeps purge progress
  only in the active call. A retry after response loss cannot recover the removed catalog identity.
  Multiple physical namespaces can exist for one logical property, while the remaining-reference
  decision can skip or target only one namespace: dropping one of several indexes on a shared
  property skipped the purge entirely, orphaning the dropped namespace immediately, and the final
  drop purged only its own namespace.
- **Expected or needed behavior:** Every unreferenced PhysicalIndexId must have a durable,
  resumable purge identity and cursor until graph-index confirms cleanup; a shared logical property
  must not leak a distinct physical namespace after its final reference is removed.
- **Resolution:** `DROP INDEX` now co-writes the catalog removal with a durable Router-owned
  retirement record keyed by `PhysicalIndexId` (`ROUTER_INDEX_RETIRED`, MemoryId 54) that freezes
  the drain target set resolved before the destructive mutation and persists one resume cursor per
  pending target. Posting scopes are disjoint per physical namespace and unreachable once the
  catalog row dies, so every dropped definition retires unconditionally — the wrong
  remaining-reference gate is deleted. The inline fast path drains bounded rounds inside the DDL;
  holds defer to a new recovery-timer lane (`index_retirement.rs`, Driver 4) advancing one bounded
  step per pending target per tick; records delete exactly when the last frozen target confirms
  done. `DROP INDEX` returns success after the co-write and never fails on purge transport errors.
- **Owning regression tests:** PocketIC
  `dropping_one_of_two_indexes_on_shared_property_does_not_leak_its_namespace` (per-namespace leak,
  plus sibling-survival guard) alongside the restored `drop_index_purges_postings_from_graph_index`
  and timer-compaction contracts in `adr0023_index_store_consistency.rs` (6 passed); router unit
  tests in `router/src/index_retirement.rs`: response-loss hold-and-converge, cross-pass cursor
  persistence, per-target partial completion, empty-pending retirement, scan pagination; memory
  layout inventory updated to 55 regions.
- **Evidence:** Failure modes F1–F5 traced through `router::drop_index` → `drop_named_index`
  (catalog row removed first) → call-local resume loop over `graph_index_lookup_targets`; probe
  verified postings are keyed per `(physical_index_id, property_id)` so per-namespace retirement is
  complete by construction. The Quint model named in the next decision is unnecessary: the
  retirement record is a single-owner monotonic lifecycle (enqueue → per-target drain → delete)
  with no concurrency beyond the idempotent bounded steps, which the unit regressions cover.

### GAP-2026-08-11-004 — Current V1 metadata boundary leaves too little extension space

- **Status:** Resolved (obsolete)
- **Severity:** P2 / Persisted-layout capacity
- **Owner:** `ic-stable-clustered-hash-map` header and entry-address invariants
- **Observed behavior:** The current V1 metadata fields occupy bytes 0 through 62 and the table
  entry boundary is now defined at byte 128. Fresh layout creation clears the complete metadata
  extension.
- **Expected or needed behavior:** Future persisted metadata must fit within the 128-byte current
  V1 prefix without moving entries again.
- **Evidence:** `header.rs::HEADER_SIZE`, `header.rs::DATA_OFFSET`, and the test
  `current_v1_header_uses_128_byte_data_boundary` establish the boundary and initialization.
- **Impact:** The map has space for additional persisted state while retaining the V1 version
  number and fixed field offsets. The extra prefix is allocated once per map and does not change
  entry stride or mutation algorithms.
- **Next decision:** **Obsolete.** The `ic-stable-clustered-hash-map` crate is retired; its last
  consumer (`VECTOR_PARTITION_HEADS`) migrated to `ic-stable-linear-hash-map` (ADR 0067) and the
  crate was removed from the workspace. This gap no longer applies.

### GAP-2026-08-11-002 — Active-remap overflow can force a full remap drain in one insert

- **Status:** Resolved (obsolete)
- **Severity:** P1 / High bounded-execution risk
- **Owner:** `ic-stable-clustered-hash-map` capacity, incremental-remap, and mutation-atomicity
  invariants
- **Observed behavior:** The header stores logical capacity at offset 29. `size_up()` accepts only a
  settled table, preserves the existing tail reserve while doubling buckets, and contains no
  active-remap drain. Normal and remap relocation preflight the full chain and extend a cleared
  64-slot tail before the first destructive write. `REMAP_BATCH = 64` counts every examined
  position. Public `insert` and `remove` keep the direct path when the final `REMAP_BATCH + 1` slots
  are empty, which bounds all maintenance relocations plus the requested insert without growth.
  Other growth-capable operations write directly through a block-granular undo transaction. It
  snapshots each overwritten block below the operation's initial logical capacity at most once,
  delegates exact growth and writes to the backing memory, and restores those disjoint blocks if the
  operation returns an error. Writes in a newly grown logical tail need no snapshot because restoring
  the original header makes them unreachable.
- **Expected or needed behavior:** No public mutation should perform a table-sized remap fallback.
  Capacity pressure must be handled before relocation writes begin, preserve the current two-mapping
  (`N` / `N-1`) mixed-lookup contract, and leave the key set and header recoverable if stable-memory
  growth fails. A failed public mutation must return the exact maintenance error with the whole
  operation's logical bytes, header, length, capacity, remap boundary, and key set unchanged.
- **Evidence:** `header.rs::CAPACITY_OFFSET` and `map.rs::capacity` establish the persisted source of
  truth. `extend_tail`, `insert_and_relocate`, `remap_position`, and settled-only `size_up` enforce
  grow -> clear -> publish ordering and preserve N/N-1 lookup.
  `multi_boundary_active_remap_insert_oom_is_operation_atomic_and_retry_succeeds` and
  `multi_boundary_active_remap_remove_oom_is_operation_atomic_and_retry_succeeds` prove exact-byte,
  header, key-set, reopen, and successful-retry behavior when a later remap boundary needs growth.
  The preceding persisted unfiltered `canbench` run uses `DefaultMemoryImpl` and measures the
  generic clustered insert at 67.62M instructions on the wasm32 canbench target; the focused
  active-remap fixtures also complete under the target-selected stable-memory backend. A
  post-change non-persist check on 2026-08-11 measured 75.26M for the generic insert, 37.88K for
  the active batch, and 48.43K for active tail extension; the checked-in artifact was not rewritten
  by that check. These values are not directly comparable with the former explicit-`VectorMemory`
  artifact.
- **Impact:** The table-sized active-remap fallback remains removed. A public `insert` or `remove`
  now either commits its bounded maintenance and requested mutation together or returns
  `OutOfMemory`/`CapacityOverflow` without changing logical map state. Stable-memory page count is
  physical allocation and need not shrink if an earlier grow succeeded before a later failure. The
  undo transaction covers returned errors; traps rely on the IC message rollback boundary, and the
  map adds no standalone write-ahead journal for process-crash recovery outside that boundary.
- **Next decision:** **Obsolete.** The `ic-stable-clustered-hash-map` crate is retired; its last
  consumer (`VECTOR_PARTITION_HEADS`) migrated to `ic-stable-linear-hash-map` (ADR 0067) and the
  crate was removed from the workspace. This gap no longer applies.

### GAP-2026-08-11-003 — Settled `size_up` clears a capacity-scale region in one mutation

- **Status:** Resolved (obsolete)
- **Severity:** P1 / High bounded-execution risk
- **Owner:** `ic-stable-clustered-hash-map` settled resize initialization and persisted-capacity
  invariants
- **Observed behavior:** Active remap maintenance is bounded to `REMAP_BATCH = 64` positions and
  production no longer drains a whole active remap. A settled threshold insert now persists a
  target capacity and clear cursor, grows/clears only a bounded prefix, and retains the old N until
  publication. Physical growth is also extended incrementally as the cursor advances. Settled
  inserts now guard lookup/relocation at 4,096 occupied entries; an over-budget request advances
  one persisted resize/remap step and returns `InsertError::RelocationBudgetExceeded` before the
  request mutates the table.
- **Expected or needed behavior:** The public mutation path must have a per-call initialization bound
  independent of table capacity. New-region initialization is resumable and persisted: each
  operation clears only a bounded chunk, reopen resumes from the cursor, the new bucket mapping is
  not published before the entire region is initialized, and every pending mutation advances the
  cursor or completes the resize.
- **Evidence:** The owning paths are `header.rs::ResizeState`, `map.rs::advance_resize_initialization`,
  `map.rs::publish_resize`, and `map.rs::clear_region`. The deterministic tests
  `threshold_resize_clears_a_bounded_prefix_and_reopens`,
  `pending_resize_clear_oom_rolls_back_cursor_and_reopens`,
  `publishing_resize_marker_reopens_and_finishes_metadata_commit`, and
  `clear_new_aborts_pending_resize_and_reopens_settled` cover cursor progress, reopen, OOM rollback,
  metadata recovery, and reset behavior. The updated N=13/N=16/N=20/N=23 threshold fixtures keep
  setup outside the timed closure and each measures 17,635 scoped instructions for the bounded
  64-slot prefix; the exact persisted values are in
  `crates/ic-stable-clustered-hash-map/canbench_results.yml`. The added settled collision-chain
  sweep measures exact relocation of 4, 64, 256, 1,024, and 4,096 occupied slots. The subject-width
  model (13-byte key plus 41-byte value) measures 600,576 / 2,390,784 / 9,551,635 instructions at
  256 / 1,024 / 4,096 slots, respectively, with an empirical fit of approximately `2,331 * C +
  3,837`. The fixture is setup outside the timed closure and verifies every resident after the
  insert; it is a map-algorithm model, not a vector-canister call-path benchmark. The regression
  tests `settled_chain_budget_starts_persisted_resize_and_retry_advances` and
  `settled_chain_budget_recovery_oom_is_atomic` cover persisted progress, reopen, and exact-byte
  rollback at the new boundary. The post-change non-persist threshold check measured 18.10K scoped
  instructions for each N=13/N=16/N=20/N=23 fixture; the persisted artifact remains the preceding
  17.635K baseline until an explicit final persist run. The bounded insert path now reuses the
  cluster-end boundaries collected by its preflight. A new 4,096-entry single-cluster fixture
  measured 950.56K scoped instructions with the plan versus 2.05M in a controlled plan-disabled
  run (approximately 53.6% lower); this exploratory benchmark is not persisted yet.
- **Impact:** The capacity-scale `clear_region` write is no longer performed in one public mutation,
  and the target mapping is not advertised until the cursor reaches the target. The scale benchmark
  still includes target-selected stable-memory growth and transaction overhead. Settled and active
  relocation attempts now have a deterministic 4,096-occupied-entry guard; the returned error
  commits bounded persisted maintenance while the request key/value remains the caller's
  responsibility. Active remap persists a lower scan cursor when a lower range can be processed,
  but a blocked boundary is retained rather than skipped. Stable-memory pages grown before a later
  returned error remain physical backing outside the logical rollback contract.
- **Next decision:** **Obsolete.** The `ic-stable-clustered-hash-map` crate is retired; its last
  consumer (`VECTOR_PARTITION_HEADS`) migrated to `ic-stable-linear-hash-map` (ADR 0067) and the
  crate was removed from the workspace. This gap no longer applies.

### GAP-2026-08-11-001 — Inner resize leaves the pending insert distance stale

- **Status:** Resolved by commit `1efac368d` (crate retired; see ADR 0067)
- **Severity:** P1 / High
- **Owner:** `ic-stable-clustered-hash-map` inner insert resize and pending-entry distance
  invariant
- **Observed behavior:** In the defective inner `insert_and_relocate` resize branch,
  `size_up()` recalculates the pending entry's bucket and insertion position under the grown
  mapping but leaves `entry.distance` from the prior mapping. The public insert returns `Ok`,
  `len` increases, and iteration contains the entry, while `get` and `contains_key` fail both
  immediately and after re-open.
- **Expected or needed behavior:** After an inner resize, the pending entry's stored distance
  must be the checked distance from its recomputed bucket to its recomputed position before
  relocation or the terminal write continues.
- **Evidence:** `crates/ic-stable-clustered-hash-map/src/map.rs::insert_and_relocate` is the
  owning branch. The repair adds
  `entry.distance = checked_distance(position - b)` immediately after the recomputed bucket and
  position. The focused regression target
  `map::tests::inner_resize_recomputes_relocated_entry_distance` covers successful insert,
  `len`/iterator presence, and immediate plus `init`/re-open point lookup. The repair and
  regression passed the focused Rust test, full crate library tests, format/check/clippy gates,
  and independent review. The repair is recorded in commit `1efac368d`.
- **Impact:** A successful public insert can persist an entry that iteration exposes but point
  lookup cannot reach, and the inconsistency survives re-open.
- **Next decision:** None for this defect; retain the focused regression with the owning map
  implementation.

### GAP-2026-08-10-001 — Future row-copy migration must carry the I8 per-row scale

- **Status:** Open
- **Severity:** P2 (latent; no code path today)
- **Owner:** vector canister slab page store / migration primitive
- **Observed behavior:** With `VectorEncoding::I8` implemented (Slice 0242), stored rows are an i8
  payload plus a per-row f32 scale in row-meta aux. The design's row-copy migration primitive
  (`export_vector_rows` / `import_vector_rows`) is **not implemented**; grep confirms no such code
  exists yet.
- **Expected or needed behavior:** A future export/import that copies rows across canisters (shard
  split, ADR 0031 multi-canister) must carry the per-row aux (scale) alongside the payload, or the
  imported I8 rows would be decoded with a wrong/zero scale.
- **Owner:** vector canister `PAGE_STORE` / `export_vector_rows` future slice.
- **Evidence:** `crates/vector-canister/src/facade/stable/page_store.rs` (`read_row_bytes` returns
  `(vertex_id, bytes, aux)`); `design/index/vector-index.md` §Multi-canister readiness.
- **Impact:** Latent until migration is built; adding it without carrying aux would silently corrupt
  I8 distances on the target canister.
- **Next decision:** When the migration primitive slice is planned, require `export_vector_rows` /
  `import_vector_rows` to round-trip the row aux.

### GAP-2026-08-10-002 — `size_up` under-allocates the canonical entry stride

- **Status:** Resolved by commit `c1dc31db7` (crate retired; see ADR 0067)
- **Severity:** P1 / High
- **Owner:** `ic-stable-clustered-hash-map` entry allocation/layout and `size_up`
- **Observed behavior:** Before the repair, `entry_stride()` used the canonical
  `key_size + value_size + 4` layout, but `size_up` computed its growth target with a stale
  `key_size + value_size + 2` stride.
  At the `n = 13 → 14` capacity of 16,398, the stale `+ 2` target under-requests the
  canonical allocation by `2 * capacity = 2 * 16,398 = 32,796 bytes`.
- **Expected or needed behavior:** Every growth target must cover the canonical allocation
  used by `entry_offset` / `entry_stride`, before clearing or writing the new region.
- **Evidence:** `crates/ic-stable-clustered-hash-map/src/map.rs::size_up` now derives the growth
  target from `self.entry_stride()`. The
  `load_threshold_resize_allocates_the_canonical_entry_stride` regression starts from fresh
  minimal-page backing, deterministically seeds a collision-free `n = 13` table at the normal
  75% load threshold, triggers growth through public `insert`, and asserts that the backing covers
  `DATA_OFFSET + capacity * entry_stride`. The focused test failed before the repair with
  `VectorMemory` `write: out of bounds` and passes after it.
- **Impact:** Before the repair, pre-grown backing could mask the physical out-of-bounds access,
  and collisions could trigger the stale-stride path earlier. Under a conforming `Memory`
  implementation whose out-of-bounds access traps or panics, the failure does not silently corrupt
  the pre-existing old-table bytes; this conclusion does not cover non-conforming memory
  implementations. The repaired path allocates from the canonical stride before clearing or
  writing the grown region.
- **Next decision:** None for the implementation. Record the primary-owned fixing commit after it
  is created.

### GAP-2026-08-04-002 — Rust canister bindings for record parameters lack a Candid form

- **Status:** Resolved
- **Severity:** P2 generated-code compile gap for `Record` parameters only
- **Owner:** `gleaph-codegen` rust-canister profile and `gleaph-cdk` record parameter type
- **Observed behavior:** The rust-canister profile always derived `CandidType` on generated
  `*Params` structs. Record-typed parameters use `gleaph_cdk::GqlRecord` (= `Vec<(String,
GqlValue)>`), whose `GqlValue` element type is candid-free by design in `gleaph-gql-value`,
  so a manifest with a Record parameter emitted bindings that failed to compile with
  `E0277: the trait bound GqlValue: CandidType is not satisfied`.
- **Resolution:** The rust-canister profile now emits `*Params` structs with only
  `#[derive(Clone, Debug)]` — no `CandidType`, no serde. Parameters never cross a candid or
  serde boundary; the generated `into_gql_params()` converts them to ordered logical GQL
  parameters at call time, which also allows raw `f128` / `f256::f256` fields for
  `Float128` / `Float256` parameters. Generated `*Row` structs and `PreparedResponse` derive
  `CandidType` + serde, with exotic row fields bound through the cdk row-binding wrappers
  (`GqlInt256`, `GqlUint256`, `GqlDecimal`, `GqlFloat16`, `Float128`, `Float256` in
  `gleaph-gql-ic-wire`, each with candid + serde impls; `Principal` is candid-native), so a
  canister can return prepared rows directly over its candid interface.
- **Evidence:** `crates/codegen/src/rust/canister.rs` (`canister_param_rust_type` emits
  `gleaph_cdk::GqlRecord`); scratch compile at `/tmp/gleaph-exotic-check` with the
  `exotic_manifest.json` (Int256/Uint256/Decimal/Principal/Float16/Float256 params and rows)
  compiles and round-trips a `PreparedResponse<FindExoticRow>` through `candid::encode_one` /
  `candid::decode_one`.
- **Impact:** Resolved for all parameter and column types; `Record` parameters compile because
  `*Params` no longer requires `CandidType`.
- **Next decision:** None — resolved in the real-type row-binding refactor.

### GAP-2026-07-25-002 — Tombstone-heavy OFFSET scans lack a persistent skip structure

- **Status:** Deferred after bounded research (2026-08-23; ADR not justified). Contract-equivalent
  candidates cleared the query threshold, but the winning query-only candidate remains slower than
  the existing owner's compacted query and omitted integrated costs prevent a substitution claim.
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
  dense fast path is intentionally limited to `live_degree == logical_extent`. Plan 0283 measured
  872,634 instructions at extent 8,192 / 87.5% tombstones / OFFSET 960 / LIMIT 32, versus 38,051
  at the same density and offset 0 and 27,146 for the dense OFFSET control. Fair benchmark-only
  end-to-end probes performed canonical stable-row decoding, liveness checks, slot reconstruction,
  and identical visitor work: fixed-block counts measured 35,620 / 65,776 instructions at 75% /
  87.5%, while triggered two-level counts measured 32,138 / 59,210. The exact temporary patch and
  raw canbench CSV are retained under `design/investigations/artifacts/`. The bounded
  production-owned maintenance drain took 831,216,051 instructions across 1,025 one-work-item
  calls, then restored the query to 27,095 instructions. The sign-corrected optimistic query-only
  crossover is `Q_upper = 831,216,051 / (59,210 - 27,095) = 25,882.49 queries`. This upper bound
  omits candidate build, mutation, update, validation, framing, and repair costs, while compaction
  retains independent storage obligations; a positive crossover alone does not establish
  substitution or workload benefit. The stale predecessor wording was accidentally included in
  unrelated commit `b787ac389`; this hunk records corrected evidence without rewriting that commit.
- **Impact:** Large sparse buckets can spend instructions scanning tombstones for OFFSET/LIMIT.
  Introducing durable metadata now would expand the storage format and add mutation,
  compaction/rebuild, reopen, and benchmark obligations before the design is understood.
- **Next decision:** Retain the exact sparse scan and current compaction ownership; do not open an
  ADR from this slice. Revisit only if integrated candidate build/mutation/update/validation costs
  and a demonstrated workload establish a substitution benefit beyond compaction's query and
  storage obligations. Any adoption remains a separate ADR/storage slice.

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

### GAP-2026-07-11-005 — `FreeSpanStore` reopen validation has no production-scale cost bound

- **Status:** Resolved (2026-08-22; declared reopen envelope with measured scale probes — up to
  262,144 non-coalescing active spans per store must validate within 1.0B wasm instructions and
  ≤8 MiB transient heap; measured 774.92M instructions at the declared maximum, essentially linear
  at ~2,956 instr/span, so the existing fail-closed sorted-merge validator is accepted unchanged
  and no ADR 0007/0039 amendment is required. Owning benchmarks:
  `bench_lara_free_span_store_reopen_16384/65536/262144` in
  `crates/ic-stable-lara/src/lara/edge/free_span/bench.rs`, thresholds documented in the bench
  header and [storage/lara.md](storage/lara.md) "Reopen integrity". Fragmentation beyond the
  declared level is outside the supported upgrade envelope rather than trapped.)
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

- **Status:** Resolved (2026-08-22; eager promotion — the labeled insert dispatcher promotes a
  non-tail default-bypass row to bucket mode via the existing preflighted
  `promote_bypass_to_bucket_mode` transition before its next same-label insert, so slab-region
  extension and the successor-origin bump only ever run while the row is the tail. Measured scale
  probes `bench_labeled_non_tail_bypass_insert_256/1024/4096`: pre-fix 1.18M / 4.40M / 17.25M
  instructions for 16 inserts (linear, ~263 instr/successor-row), post-fix 343K / 363K / 382K
  (flat; +11% for 16× successors, bounded by the owning PMA leaf). Scan semantics reuse the
  established promotion transition; no persisted row meaning, scan geometry, or PMA ownership
  change, so no dedicated ADR is required. Owning tests:
  `same_label_insert_into_non_tail_bypass_row_promotes_to_bucket_mode` (fails if the promotion
  guard is removed — row would stay default-edge-labeled) and
  `same_label_insert_into_tail_bypass_row_stays_in_bypass_mode`; contract documented in
  [storage/lara.md](storage/lara.md) "Labeled LARA". A `debug_assert` on
  `may_use_homogeneous_bypass` now guards `insert_homogeneous_bypass_edge` directly.)
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

- **Status:** Resolved (2026-08-22; bounded heap cache
  `PREPARED_SORTED_CACHE` keyed by `(PreparedPlanKey, PreparedSortSignature)` — the signature is
  validated once by `normalize_prepared_sort` (the single source of truth shared with AST
  injection): direction text canonicalized case-insensitively, allowed keys exact-matched and
  deduplicated, term order preserved because `ORDER BY a, b` and `ORDER BY b, a` are different
  plans. Cap 128 entries with smallest-key eviction on insert; invalidated on upsert replacement,
  `drop_prepared`, and post-upgrade rebuild (variants rebuild lazily). Stable storage keeps only
  query source + metadata per ADR 0053's heap-cache principle, so no stable-layout change.
  Measured `bench_prepared_sorted_variant_rebuild` 318.12K vs
  `bench_prepared_sorted_variant_cache_hit` 29.50K instructions (~10.8x reduction).
  Owning tests: hit-path mutation discriminator (`sorted_variant_cache_hits_normalized_signature`
  fails if the wrapper re-plans), distinct term-order/direction entries, upsert/drop/upgrade
  invalidation, cap eviction, and unchanged validation-error contracts. Router unfiltered
  `canbench --persist` deferred while the crate's benchmark artifact carries a parallel stream's
  in-flight measurements; resume with `cd crates/router && canbench --persist`.)
- **Severity:** P2 prepared-query performance gap
- **Owner:** Router prepared-query heap cache
- **Observed behavior:** The Router keeps the unsorted prepared plan in the heap cache. When a caller supplies a non-empty sort specification, the Router validates the metadata and rebuilds a derived plan for that invocation; equivalent sort specifications are not cached as separate heap entries.
- **Expected or needed behavior:** Repeated executions of the same prepared operation with the same normalized sort specification should reuse a heap-cached derived plan while stable storage continues to retain only the query source and metadata.
- **Evidence:** crates/router/src/prepared.rs::prepare_sorted_cache and the prepared query local E2E in crates/codegen/e2e/test.mjs.
- **Impact:** Sort-enabled prepared queries pay the planning and plan-encoding cost on every execution. This preserves correctness and upgrade safety but can increase latency and instruction usage for frequently reused sort variants.
- **Next decision:** Define a bounded cache key and eviction policy, including direction normalization, key ordering, metadata changes, and post-upgrade invalidation, then add hit/miss tests and a focused benchmark before introducing durable or unbounded derived state.
- **Related contracts:** ADR 0053 and crates/prepared-api/src/lib.rs

### GAP-2026-07-04-001 — Prepared execution still requires graph visibility

- **Status:** Closed by ADR 0063 (2026-08-06)
- **Severity:** P2 product gap; P1 if a public frontend must call Router prepared queries directly
- **Owner:** Router prepared catalog resolution and graph authorization
- **Observed behavior:** `authorize_prepared_execute` permitted the default Router `Executor` role,
  including an anonymous caller, but prepared-plan resolution searched only graphs visible to the
  caller. A principal that is not the graph owner or in the graph `admins` set therefore could not
  resolve the prepared plan. The social demo test had to add its application caller to the graph
  administrators while leaving its Router role at `Executor`.
- **Resolution (ADR 0063, 2026-08-06):** prepared queries are addressed by global name and bound to
  a graph at registration time; `authorize_prepared_execute` and `resolve_prepared_graph_id` were
  removed, so any caller can execute an administrator-registered prepared query. Caller-aware
  authorization (public vs visible graphs, per-query permissions) is deferred to a future
  access-control ADR.
- **Historical evidence:** `crates/router/src/rbac.rs::authorize_prepared_execute`,
  `crates/router/src/prepared.rs::resolve_prepared_graph_id` (both removed by ADR 0063), and the
  shared `install_single_shard_federation_with_graph_admins` fixture in
  `crates/pocket-ic-tests/src/lib.rs`.
- **Historical workaround (removed 2026-08-06):** `crates/social-demo-gateway` was an
  application-owned canister with a fixed scenario enum. The Gateway principal was registered as a
  graph administrator so Router could resolve the prepared plan, but it remained a default Router
  Executor with no ad-hoc `Read` role. Anonymous callers executed the fixed scenarios through the
  Gateway; Router observed the Gateway principal, not the original caller. This was an
  application-layer trusted-deputy pattern, not a product change to Router prepared-query
  authorization. The crate was removed together with the SDK-direct frontend; the frontend now
  calls Router `prepared_query` by name through `@gleaph/sdk`.
- **Related contracts:** [security/rbac-and-prepared.md](security/rbac-and-prepared.md),
  [demo/social-graph-rag.md](demo/social-graph-rag.md)

### GAP-2026-08-01-001 — Social-demo load artifacts were not reproducible from build-config.mjs

- **Status:** Resolved by Plan 0204 on 2026-08-02 UTC
- **Severity:** P2 demo-tooling drift; blocks trusting `pnpm run build:config` as an idempotent
  regeneration step
- **Owner:** `demo/social/scripts/build-config.mjs` and the committed seed/avatar
  artifacts
- **Resolution:** The generator now emits one ordered NDJSON pair (`seeds/vertices.jsonl` +
  `seeds/edges.jsonl`) directly from the canonical
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

### GAP-2026-08-04-001 — `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` is defined but never consumed

- **Status:** Resolved (2026-08-22; option (b) implemented. The between-chunk loop in
  `execute_prepared_mutation` now runs the ADR 0042 between-wave check before every chunk via
  `graph_batch_chunk_within_update_budget`, composing the crate's generalized `should_cutoff`
  predicate with ceiling `MAX_UPDATE_CALL_INSTRUCTIONS` (40B), the new
  `ROUTER_BATCH_CHUNK_WORK_INSTRUCTION_ESTIMATE` (50M: measured worst per-chunk Router work
  37.34M from GAP-2026-07-17-001 evidence plus ~34% margin), and the previously-dead
  `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` (4B) as the finalization reserve — so the constant
  is consumed by the boundary it claims to protect, and the Router provably stops dispatching
  rather than trapping at 40B regardless of Graph behavior. When the guard trips, deferred
  operations surface as per-operation errors ("router update instruction budget guard stopped
  chunk dispatch") through the same recovery path as chunk failures; resubmission completes
  them idempotently via the mutation journal. The live counter comes from the now-ungated
  `current_instruction_counter` (host builds return 0, so native tests exercise the untripped
  path). Owning tests: `chunk_budget_allows_a_fresh_message`,
  `chunk_budget_trips_at_and_past_the_exact_boundary` (pins the exact `>=` trip point against
  dropped/reordered terms or a flipped comparison),
  `chunk_budget_trips_when_only_headroom_remains`. Residual risk: loop wiring itself is not
  natively testable without an inter-canister seam; the pure guard is fully pinned.)
- **Severity:** P2 router budget-integrity gap (no current runtime risk; the shared headroom covers the
  path)
- **Owner:** `gleaph-instruction-budget` constants and the Router chunk-dispatch loop
  (`crates/router/src/gql.rs` `execute_prepared_mutation` / `graph_batch_chunk_len_for_dispatches`)
- **Observed behavior:** `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` (4B) is defined in
  `gleaph-instruction-budget` and re-exported from `gleaph-graph-kernel`, but no code path consumes
  it: the Router chunk decision caps at `max_operation_count(500M, 35B)` = 70 operations per chunk
  using the shared `MAX_DYNAMIC_UPDATE_INSTRUCTIONS`, and the between-chunk loop
  (`execute_prepared_mutation`) has no Router-local call-context cutoff. The 4B constant appears only
  in the definition, the re-export, the compile-time assertion, and the GAP acceptance bench comment.
- **Expected or needed behavior:** every defined headroom must either be consumed by the owning
  boundary or be removed. If the Router is to guarantee it never crosses the 40B limit on its own
  account (distinct from the Graph-side 35B budget inside each dispatched batch), the chunk loop
  needs the ADR 0042 original between-wave check: read the Router call-context instruction counter
  and stop before starting another chunk when `used + next-chunk estimate + reserve ≥ 40B`.
- **Evidence:** `crates/instruction-budget/src/lib.rs` (definition + assert),
  `crates/graph-kernel/src/lib.rs` (re-export), `crates/router/src/gql.rs`
  (`graph_batch_chunk_len_for_dispatches` uses only the shared budget); measured per-chunk decision
  cost 6.01M / 37.34M instructions and the ≤2 MiB inter-canister sizing bound
  (`crates/router/src/bench.rs`, `crates/router/canbench_results.yml`, GAP-2026-07-17-001).
- **Impact:** no current runtime risk: the Router's actual safety is the shared 5B
  `UPDATE_CALL_INSTRUCTION_HEADROOM` (35B dynamic budget), and the measured per-chunk work
  (decision ≤ 37.34M + ≤2 MiB encode + response) is ≪ 1% of it. The gap is integrity and
  future-proofing: the dead constant invites a false sense of protection, a future Router-side
  bounded loop cannot reference a consumed budget, and the GAP acceptance table lists a value that
  is not wired to the path it claims to reserve.
- **Next decision:** choose one of (a) remove the constant and the ADR 0060 §1 list entry, noting
  in GAP-2026-07-17-001 that the shared 5B covers the Router chunk path; or (b) add the
  between-chunk call-context cutoff to `execute_prepared_mutation` and re-derive the constant from
  measurement (≈1B suffices: decision 37.34M + response at ~7× margin). Option (b) is preferred so
  the Router itself provably never traps at 40B regardless of Graph behavior.

### GAP-2026-08-23-001 — Native `gleaph-router` lib suite has pre-existing ic0-context failures that poison unrelated tests

- **Status:** Partially resolved 2026-08-23; remaining entries Open
- **Severity:** P3 test-harness (the ic0/build items below were P1 while present)
- **Owner:** `gleaph-router` native unit test harness; `gleaph-graph` expand executor and adr0034 fixture own their respective items

#### Resolved 2026-08-23

1. **Workspace build breakage (8e392c127 → ac380c4bb).** `8e392c127 feat(index): wire edge range pushdown end to end`
   added `collect_edges_matching_indexed_property_where` call sites in
   `crates/graph/src/index/edge_lookup.rs` but omitted the collector's definition in
   `crates/graph/src/facade/store/edge_properties.rs`, leaving every commit from 8e392c127 through
   463478b44 unable to build `gleaph-graph`. Fixed by `ac380c4bb fix(graph): add the predicate-scanned
   edge property collector`.
2. **Directed equality-index candidates lose inline bytes (`b787ac389` → 317517074).**
   `b787ac389 fix(index): own edge posting label identity in wire space` let directed postings reach
   the equality-index candidate path for the first time, but that path read slots through
   `read_out_edge_slots_for_label`, which restores topology only — scanned edges carried empty inline
   property bytes, failing projected inline reads. The LARA slot-targeted reader is topology-only by
   design (`read_edge_state_at_slot` restores no payload); byte restoration lives in the batch walk.
   Fixed by `317517074 fix(graph): restore inline bytes in equality-index candidates`: the PointingRight
   arm now walks the labeled adjacency once with batch-restored bytes and keeps the posted slots,
   mirroring the PointingLeft arm. Owning test: `indexed_edge_equality_expand_return_inline_property`.

#### Remaining open

3. **ic0-context panics poison the router lib suite (this host).**
   `cargo test -p gleaph-router --lib` fails three tests in this macOS host environment:
   `facade::store::schema_migration::tests::unregistered_create_graph_migration_fails_closed_without_provisioner`,
   `provisioning::graph::tests::admission_fails_closed_for_unregistered_name_without_provisioner`,
   and `provisioning::graph::tests::admission_short_circuits_registered_name_without_provisioner`,
   all panicking with `canister_self_size should only be called inside canisters` (`ic0` system call
   reached outside canister execution), in full runs and in isolation. Those panics poison a shared
   outbox/registry test lock, cascading into
   `vector_sync::tests::{frontier_response_loss_retains_exact_marker_snapshot,
   resolved_rows_transition_to_awaiting_frontier_before_publish}`, which pass when run alone.
   Next decision: gate wasm-only tests to a canister target or inject a self-size stub, and stop one
   panic from poisoning sibling tests.
4. **var_len group-variable inline reads fail under default-enabled cypher (`b13d427ca`).**
   `gql_var_len_where_inline_property_filters_on_last_hop_edge` ("property access on group edge variable
   'e.distance' requires element indexing") and `gql_var_len_return_inline_property_decodes_indexed_last_hop_edge`
   (decodes to Null) fail once the cypher dialect is enabled by default — bisect shows both green at
   da665a70b and red at b13d427ca, i.e. newly *exposed*, not regressed, by the dialect gate. The planner
   emits var_len plans whose group-edge-variable property access needs element indexing support.
   Next decision: implement hop-aux element indexing for group edge variables or reject those property
   accesses at plan time with an explicit unsupported error before this slice lands.

The adr0034 fixture item that formerly appeared here as "Remaining open" item 5 now lives in its own
entry, [GAP-2026-08-23-002](#gap-2026-08-23-002--adr0034-e2e-fixture-predates-typed-schema-endpoint-enforcement).

#### Evidence summary

- Build breakage and B1: baseline reproduction with working-tree changes stashed at HEAD 463478b44;
  worktree bisect across 8e392c127 / cb156b0b7 / b787ac389 / b6fe10612 with the missing collector copied
  forward; candidate-path disable experiment turned B1 green, confirming the reader as the defect site.
- ic0 panics reproduce on clean 463478b44; isolated vector_sync runs are green.
- adr0034 failure persists with the slice-4 planner changes reverted, so planner anchor selection is not
  involved; see GAP-2026-08-23-002 for its owning entry.

### GAP-2026-08-23-002 — ADR 0034 E2E fixture predates typed-schema endpoint enforcement

- **Status:** Resolved (2026-08-23) — both `adr0034` read fixtures were reproduced against the
  typed-schema endpoint gate, aligned with their declared graph types without weakening validation,
  and rerun green (struct in `8f3f5c1da`; scalar in the closing commit).
- **Severity:** P1 E2E correctness-gate blocker (keeps the adr0034 PocketIC target red); not a
  production storage/query defect.
- **Owner:** the stale `adr0034` read fixtures, which insert vertices whose labels contradict their
  declared graph types. The typed-schema endpoint check promoted at Router ingress is correct
  fail-closed behavior and must not be weakened. Planner anchor code is not involved.

#### Observed behavior

`cargo test -p gleaph-pocket-ic-tests --test adr0034_inline_edge_struct_read_access` failed at HEAD
`9a4b94892` (and on the clean slices 1–3 baseline `463478b44`) with:

```
gql_query rejected: InvalidArgument("validation error: edge `:AFFINITY` cannot connect :ProjectionSource to (unlabeled) (schema constraint violation)")
```

The DDL declares `NODE User AS user ... CONNECTING (user -> user)`, but scenarios inserted sources
labeled `ProjectionSource`/`FilterSource`/… and unlabeled targets, so every MATCH violated the
declared endpoint set. `cargo test -p gleaph-pocket-ic-tests --test
adr0034_inline_edge_scalar_access` fails identically for `:ROAD` over `road_type`.

#### Fix evidence (2026-08-23)

`crates/pocket-ic-tests/tests/adr0034_inline_edge_struct_read_access.rs` was aligned with its
declared graph type: vertices now carry the declared `User` label, and per-scenario independence
moved from ad-hoc source labels to unique `updated_at` windows scoped through WHERE clauses. The
endpoint validator itself is unchanged. Terminal rerun:
`cargo test -p gleaph-pocket-ic-tests --test adr0034_inline_edge_struct_read_access` → **1 passed /
0 failed**, one single-shard federation constructor, three canister installs.

The sibling scalar fixture received the same alignment in the closing commit: vertices carry the
declared `City` label, per-scenario independence moved from ad-hoc source labels to disjoint
`distance` bands (7 / 17·19 / 27·29 / 37) scoped through WHERE equality and band-range predicates,
and every row-count/order assertion keeps its original intent. The scalar expectations were also
corrected from the never-exercised `Uint64` decode to the declared-type `Uint16` wire value,
mirroring the struct fixture's `Float64` → `Float32` correction in `8f3f5c1da`. Terminal rerun:
`cargo test -p gleaph-pocket-ic-tests --test adr0034_inline_edge_scalar_access` → green after a
fresh re-run immediately before judgment (shared-target contention with parallel agents produced
one transient unrelated build failure).

#### Next decision

None — closed. Both adr0034 read fixtures now conform to their declared graph types, and the
typed-schema endpoint gate remains unchanged and fail-closed.

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
- **Owner:** Router authorization/resolution + Graph validation boundary + Vector durable store
- **Observed behavior:** before plan 0048, there was no canister API for an application or deployment
  tool to write a canonical vertex embedding. Vector-index fixtures and demos had to seed the
  derived `graph-vector-index` canister directly, bypassing Graph canonical ownership and the
  Router embedding-name catalog.
- **Expected or needed behavior:** an authorized caller should submit only graph name, opaque encoded
  vertex id, registered embedding name, and finite F32 values to the typed Router endpoint; Router
  should validate the value dimension/finiteness before stamp allocation and the Graph await, Graph
  should validate vertex/tombstone/label and payload-independent embedding metadata, and every
  allocated Router stamp should have one exact durable owner before the first Graph await.
- **Resolution:** The typed Router `ingest_vertex_embeddings` flow validates the encoded id, live
  shard, registered
  vector definition, value dimension, and finiteness, then synchronously co-writes the allocated
  stamps and exact `AwaitingGraph` intents before the first Graph await. Graph `stamp_embedding`
  validates existence/tombstone state, required labels, and payload-independent embedding
  metadata/encoding, then returns the Router-issued stamp without embedding-byte or journal writes.
  Exact acceptance transitions the row to `AwaitingVector`; exact rejection changes the row to
  `AwaitingFrontier`; an observed Vector prefix also changes only the applied rows to
  `AwaitingFrontier`. Unknown Graph, Vector, or frontier outcomes retain the applicable phase. No
  finite-time or client-level exactly-once guarantee is implied.
- **Evidence:** `crates/router/src/api/control.rs::ingest_vertex_embeddings`;
  `crates/graph/src/canister/handlers.rs::stamp_embedding`; and the focused Router/Vector unit
  coverage. The named PocketIC gate `graph_only_markerless_lane_advances_frontier_on_autonomous_timer`
  passes exactly one test with seven filtered, one `PocketIc`, one federation, and four canister
  installs; it advances the ordinary timer with an empty MemoryId 53 lane and exercises bounded GC.
  The persisted Router `crates/router/canbench_results.yml` artifact has all 41 terminal benchmark
  entries; `markerless_frontier_catalog` records exactly 52,080,138 total and 52,079,143 scoped
  instructions. The dated Plan 0277 focused/persisted Vector `bench_router_frontier_gc_budget`
  baseline records 2,110,300,092 total and 2,110,299,087 scoped instructions. The later live
  artifact currently records 2,084,327,695 total and 2,084,326,690 scoped instructions from
  unrelated work and is not Plan 0277 evidence. The targeted
  affected-crate format check passes, while the
  workspace-wide format check is blocked by unrelated dirty paths and never passed. The bounded
  Quint model has fifteen deterministic tests and 5,000 sampled traces with all requested witnesses;
  `quint verify` remains unrun and non-required, so this is not production proof or a finite-time
  liveness claim.
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

### GAP-2026-07-17-001 — Dynamic instruction-budget headrooms lack measured acceptance criteria

- **Status:** Resolved by measured canbench + PocketIC acceptance (2026-08-03/2026-08-04; commits
  `test(graph): measure plan-batch tail costs for headroom acceptance`, `test(graph): verify
plan-batch drain boundary end to end`, `test(graph-index): measure posting_batch loop costs for
headroom acceptance`, `test(vector-index): measure vector_sync_batch loop costs for headroom
acceptance`, `test(graph-index): measure batched page-answer lookahead tail`, `test(router):
measure graph batch chunk decision costs for headroom acceptance`, `test(graph): measure timer
maintenance tick and per-step costs`)
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
- **Resolution:** every dynamic batch / bounded maintenance loop in the GAP now has measured,
  path-specific instruction-ceiling and headroom acceptance evidence (plan-batch, posting_batch,
  vector_sync_batch, page-answer lookahead, Router chunk decision, timer maintenance):
  - **Graph plan-batch** (`execute_plan_update_batch`, `GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM`
    2B + 500M drain estimate): adversarial single operation 32.98M < `MIN_OP_INSTRUCTION_ESTIMATE`
    (50M) and far below the ~37.5B trap threshold; response tail ≤ 581K vs the 2.5B reserve; the
    inter-canister drain is covered by the PocketIC boundary test
    (`adr0060_plan_batch_instruction_boundary.rs`) which completes a 300-operation NEXT-chained
    mutation on a converged index without trapping and serves the drained posting.
  - **graph-index `posting_batch`** (32B ceiling, 100M reserve): max single op ≈1.92M (4096-byte
    value) + response 38K, two orders below the reserve; ceiling cuts off only after ~16.6K
    max-value postings. Fully local; no boundary test needed.
  - **graph-vector-index `vector_sync_batch`** (32B ceiling, 100M reserve): max single op ≈1.12M
    (flat in dims) + response 38K, two orders below the reserve; ceiling cuts off after ~29K
    upserts. Fully local; no boundary test needed.
  - **graph-index page-answer lookahead** (500M query lookahead): max per-check work (worst page
    ≈206.4M + encode ≤ 316K) ≈206.7M, 2.4× below the lookahead. The 1B update lookahead has no
    caller and needs no evidence until used.
  - **Router mutation-batching chunk decision** (`ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` 4B):
    decision cost 6.01M nominal / 37.34M adversarial (≈0.15% / ≈0.9% of the reserve); the
    remaining per-chunk tail is bounded by the inter-canister sizing policy (≤2 MiB) and the
    dispatch count is bounded by live shard count, so no dedicated boundary test was added.
  - **Graph timer maintenance** (`MAX_TIMER_MAINTENANCE_INSTRUCTIONS` 32B,
    `TIMER_MAINTENANCE_INSTRUCTION_HEADROOM` 100M): single work item 61.60K and whole tick on a
    dense 2048-edge hub 1.33M, so the worst per-check work (≤ whole tick) is ≈1.3% of the reserve
    and a realistic backlog drains in ≈0.004% of the cap. Fully local; no boundary test needed.
    Follow-up refactor (`relocate_edge_properties_for_move`): the inline-property posting rekey,
    previously a post-pass loop outside the budget check, now runs inside the per-move observer
    through the unified property relocation, so the entire edge-move consequence is covered by
    the per-item cutoff; the measured tick is unchanged (1.33M, re-persisted 2026-08-04).
- **Evidence:** canbench targets and persisted results: `crates/graph/src/bench/plan_batch.rs`,
  `crates/graph/src/bench/timer_maintenance.rs`, `crates/graph-index/src/bench.rs`,
  `crates/graph-vector-index/src/bench.rs`, `crates/router/src/bench.rs`, with the matching
  `canbench_results.yml` per crate; the plan-batch PocketIC boundary test
  `crates/pocket-ic-tests/tests/adr0060_plan_batch_instruction_boundary.rs`. No headroom was
  justified by copying another canister's constant: each path carries its own measured worst-case
  and tail numbers.
- **Related contracts:** [ADR 0041](adr/0041-router-graph-batch-mutation-dispatch.md),
  [ADR 0042](adr/0042-router-dynamic-instruction-budget-batching.md),
  [ADR 0020](adr/0020-deferred-maintenance-timer-drain.md),
  [design/index/property-index.md](index/property-index.md),
  [design/index/vector-index.md](index/vector-index.md)

### GAP-2026-08-07-001 — Newer-incarnation replacement can tombstone the old row before a fallible append

- **Status:** Resolved by a commit-before-tombstone reorder in `vector_upsert` (Plan 0233).
- **Severity:** P2 correctness edge (only reachable on stable-memory OOM during a subject-map commit)
- **Owner:** graph-vector-index (`VectorCanisterStore::vector_upsert`)
- **Observed behavior:** In the `mutation_id > stamp` (newer-incarnation) branch, the superseded old
  active/shadow slots were tombstoned _before_ the fallible `VECTOR_SUBJECT_TO_ID` commit. If that
  commit failed (`StableGrowFailed`, stable-memory OOM), the retained old subject entry pointed at a
  now-tombstoned row, ghosting the subject from search.
- **Resolution:** The branch now commits the subject map (pointing at the new slot) before tombstoning
  the old slots; on a commit failure it tombstones only the just-appended rows, leaving the old slot
  live and the retained entry consistent. A regression test
  (`newer_stamp_upsert_commit_failure_keeps_old_slot_live`) injects a subject-map insert failure via
  a test seam (`arm_subject_insert_failure`) and asserts the old slot stays live and `live_len` is
  unchanged. The resurrection case is likewise fixed: `unmark_deleted` now runs only after a
  successful commit.
- **Evidence:** `crates/vector-canister/src/facade/store/mutation.rs` (reorder + seam),
  `crates/vector-canister/src/facade/store/tests.rs`
  (`newer_stamp_upsert_commit_failure_keeps_old_slot_live`).
- **Related contracts:** [ADR 0032](adr/0032-vector-index-slab-page-store.md)

## Property and index capability gaps

The following property/index status was verified against the implementation at 2026-08-10 00:28:05 UTC +0000.
The canonical property-index contract remains [design/index/property-index.md](index/property-index.md);
this section records only the remaining gaps and the boundary at which each one is owned.
[ADR 0059](adr/0059-create-index-migration-backfill.md) is the accepted, partially implemented
source of truth for the migration-driven `CREATE INDEX` backfill lifecycle; this ledger does not
duplicate its state machine or ownership rules.

### Priority order

| Priority | Work item                                                                                  | Current status                                                                               |
| -------- | ------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------- |
| P0       | Backfill existing vertex, sidecar, and INLINE values before advertising an index as active | Closed — vertex/sidecar closed 2026-08-22 via GAP-2026-07-29-006; edge INLINE closed 2026-08-23 via GAP-2026-08-22-001 + GAP-2026-07-29-001 (identity rule + un-ignored cross-canister scenarios, 5/5) |
| P1       | Define INLINE removal/`NULL` transitions and complete vertex `MATCH` range planner wiring  | Closed 2026-08-22 — GAP-2026-07-29-004 resolved by contract-pinning tests (see entry); GAP-2026-07-29-002 closed 2026-08-21                                |
| P1       | Add edge range postings, Router seed planning, and execution support                       | Closed 2026-08-22 — GAP-2026-07-29-003 (see entry)                                           |
| P1       | Restore anchored multi-DML roll-forward saga convergence on both shards                    | Closed 2026-08-22 — GAP-2026-08-21-001 (see entry)                                           |
| P2       | Add vertex nested-record field indexes with a canonical dotted-path contract               | Open for slice-4 acceptance — GAP-2026-07-29-005 (ADR 0073 slices 1–3 implemented and validated; slice 4 landed on `39746f7b3`, acceptance pending Plan 0285 validation) |
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

- **Status:** Closed (2026-08-23). The Graph edge-property export enumerates both canonical
  value domains under one opaque cursor: sidecar `EDGE_PROPERTIES` rows first, then canonical
  edges carrying indexed inline values, decoded through the same `inline_index_values`
  membership resolution mutations use. The cursor carries a domain tag byte
  (`0x00` sidecar key, `0x01` inline `(wire label, owner)` position); untagged legacy cursors
  are rejected cleanly. Inline enumeration visits outgoing rows only and keeps the exact
  mutation identity — directed forward rows always, undirected max-endpoint rows only — so no
  mirror double-posting and every later Remove can address what backfill inserted. A vertex
  started within budget is collected fully (resume advances between vertices), so resume never
  skips or replays an identity; `done` is true only after both domains exhaust.
  ADR 0059's one-opaque-export consequence is now implemented as written. Owning unit tests:
  `backfill_enumerates_indexed_inline_scalar_values`,
  `backfill_emits_only_the_canonical_undirected_owner`,
  `backfill_resume_walks_both_domains_without_duplicate_identities`,
  `unversioned_backfill_cursor_is_rejected`. The deferred cross-canister create-index lifecycle
  proof (plans/0281 scenarios) initially exposed GAP-2026-08-22-001; after that identity fix
  landed (plan 0282), both un-ignored scenarios pass —
  `edge_inline_create_index_migration_converges_active_with_complete_postings` and
  `edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges` in
  `crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`, 5 passed / 0 failed at the
  closing run — completing this entry's migration-path validation over inline data, including
  equality/range completeness, per-shard coverage, multiplicity, undirected canonical-owner
  uniqueness, and same-wasm upgrade reopen.)
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
  and Active-only transition) is implemented with its cross-canister PocketIC proof complete
  (GAP-2026-07-29-006, closed 2026-08-22); this entry's remaining work is the edge `INLINE`
  enumeration itself. The existing operator cursor endpoints remain separate from that lifecycle.

### GAP-2026-07-29-002 — Normal `MATCH` planning does not select vertex range indexes

- **Status:** Closed — implemented (2026-08-21)
- **Severity:** P1 query capability
- **Owner:** Router catalog projection and GQL planner statistics boundary
- **Observed behavior:** The graph-index range API and `SEARCH` range path are present, but
  `RouterGraphStats::is_vertex_property_range_indexed` returned `false` unconditionally, so the
  normal `MATCH ... WHERE` planner could not use a vertex range index through its
  planner-statistics contract. Fixing only the stats gate surfaced the second half: the Router
  seed-anchor model had no range variant, so a leading non-Eq `IndexScan` reached graph shards
  with no index client and failed at runtime.
- **Implemented behavior:** Both halves are wired. The Router projects range capability from the
  same Active vertex catalog membership as equality (`planner_stats.rs`, fail-closed for
  unindexed properties); anchor selection lowers one-sided MATCH range predicates to
  `IndexScan` with the original predicate retained as a residual `PropertyFilter`; the Router
  seed model gained `IndexAnchor::Range` (`seed.rs::RangeSeedProbe` with `SeedRangeBound`)
  extracted from leading non-Eq `IndexScan` ops and resolved through the new
  `IndexLookup::lookup_range` paginated collector; Graph already skips any seeded leading scan op.
- **Evidence:** `crates/router/src/planner_stats.rs`, `crates/router/src/seed.rs`,
  `crates/router/src/index_lookup.rs`; planner contracts in
  `crates/gql-planner/tests/planner_tests.rs::match_range_anchor_*`; runtime contract
  `crates/pocket-ic-tests/tests/router_gql_query.rs::single_shard_vertex_index_match_range`.
- **Next decision:** None open for vertex MATCH ranges; two-sided bounds ride the same anchor via
  residual conjunction, and cross-type overscan clamping is a later performance refinement.

### GAP-2026-07-29-003 — Edge range index has no query/planner path

- **Status:** Closed — implemented (2026-08-22)
- **Severity:** P1 query capability
- **Owner:** Graph-index edge posting API, Router edge seed planning, and Graph edge candidate execution
- **Observed behavior:** Edge postings support equality lookup and paged equality lookup, but no
  `lookup_edge_range` endpoint, edge range request path, or edge range planner-statistics capability
  exists. The implemented `PostingRangeRequest` path is vertex-posting oriented.
- **Expected or needed behavior:** An edge property declared indexable should support the same
  encoded ordering contract for bounded range predicates, including label/direction scoping and
  resumable pages, before the planner emits an edge range scan.
- **Implemented behavior:** The ledger next-decision was adopted. graph-index gained the paginated
  ordered endpoint `lookup_edge_range_page`
  (`crates/graph-index/src/facade/store/edge_postings.rs`, key bounds derived in
  `crates/graph-index/src/posting_range.rs::edge_posting_key_half_open_range`) preserving the full
  `(physical_index_id, property_id, value, label_id, shard_id, owner_vertex_id, slot_index)` order
  with an in-canister wire-label sieve and the existing direction subset rule; kernel request type
  `LookupEdgeRangePageRequest` reuses `PostingRangeRequest` and the `EdgePostingCursor`. The Router
  projects edge range capability from the same Active edge catalog membership as equality
  (`RouterGraphStats::is_edge_property_range_indexed_for`, fail-closed), lowers one-sided leading
  range predicates to `IndexAnchor::EdgeRange(EdgeRangeSeedProbe)` and executes them drain+clamp:
  pages are drained within the 0270 comparison-domain interval (`gql::range_bounds_for_encoded_key`)
  and residual predicates stay as filters — no cursor state survives the call. The planner emits
  one-sided `PlanOp::EdgeIndexScan{cmp}` via the leading-edge fusion path (equality wins first),
  retaining the original predicate as a residual filter. The single-shard executor clamps through
  the shared helper and falls back to a canonical `EDGE_PROPERTIES` filtered superset scan for
  unsupported comparison domains.
- **Evidence:** storage contracts
  `crates/graph-index/src/facade/store/tests.rs::lookup_edge_range_page_*`; router contracts
  `crates/router/src/seed.rs::edge_range_scan_anchor_*`,
  `crates/router/src/gql.rs::edge_range_seed_probe_*`; planner contracts
  `crates/gql-planner/tests/planner_tests.rs::match_edge_range_anchor_*`; executor contract
  `crates/graph/src/plan/query/executor/scan/tests.rs::executes_edge_range_index_scan_with_domain_clamped_between`;
  runtime `crates/pocket-ic-tests/tests/router_gql_query.rs::single_shard_edge_index_match_range`
  and `federated_edge_index_match_range_with_domain_clamp`.
- **Next decision:** None open; `!=` complement policy remains Slice D.

### GAP-2026-07-29-004 — INLINE removal/NULL transitions need explicit posting semantics

- **Status:** Resolved (2026-08-22; contract pinned by owning tests — the implementation already
  satisfied the intended old-key/new-key semantics. Executor level: SET of top-level `NULL`
  rejects with `NullInlineProperty`, struct records missing a schema field or carrying a `NULL`
  leaf reject with `InvalidInlinePropertyValue`, and every rejection leaves the stored bytes
  untouched, so no stale posting can be produced. Store level: accepted updates dispatch exactly
  Remove(prev)+Insert(next) per membership, equal-value updates dispatch nothing, a leaf-scoped
  membership transitions only its own leaf key, and a width-mismatched update rejects before any
  posting or row change. Contract recorded in
  `design/storage/labeled-edge-inline-properties.md` §Mutation write semantics. Owning tests:
  `inline_edge_scalar_set_null_aborts_without_write`,
  `inline_edge_struct_set_missing_field_aborts_without_write`,
  `inline_edge_struct_set_null_leaf_aborts_without_write` (executor),
  `updating_indexed_inline_scalar_swaps_posting_old_for_new`,
  `updating_indexed_inline_scalar_to_same_value_emits_no_posting`,
  `updating_indexed_inline_struct_leaf_replaces_only_that_leaf_posting`,
  `updating_indexed_inline_bytes_with_wrong_width_rejects_before_write` (store). No code-path
  change, so no benchmark artifact update is required.)
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

- **Status:** Open for slice-4 acceptance (updated 2026-08-23). Slices 1–3 are implemented and
  terminal-validated (focused unit gates, graph/graph-index all-target check/clippy, focused
  canbenches, posting-level PocketIC lifecycle, and an independent detached-worktree run at the
  `aa7fd9124` baseline plus only the remediation patch); domain decision recorded in
  [ADR 0073](adr/0073-vertex-nested-property-indexes-share-dotted-path-domain.md), 2026-08-22.
  Slices 1–3 landed: kernel `IndexedVertexMembership` gained required `field_path` +
  `ancestor_property_id` (Router-interned leaf identity plus the ancestor record property Graph
  dispatches by); Router DDL interns leaf and ancestor identities and validates bounded depth
  and typed leaf kind (`MAX_VERTEX_NESTED_INDEX_SEGMENTS`, container leaves rejected at declare
  time); vertex mutation dispatch, delete planning, the index-build fence, bulk writes, and the
  repair backfill all expand through one owner
  (`crates/graph/src/property/index_dispatch.rs::vertex_posting_transitions`) over the shared
  dotted-path resolver (`crates/graph/src/property/dotted_path.rs`), riding the existing
  old-key/new-key transition contract; migration builds export nested facts through
  `CanonicalExportTarget::Vertex.record_source`. Owning tests: router DDL interning/validation
  (`nested_vertex_ddl_*` in `index_catalog.rs`), GAP-004-grade store contracts for the record
  domain (`nested_record_*` in `catalog_context.rs`), record-walking backfill/export units, and
  the cross-canister lifecycle scenario
  `vertex_nested_create_index_migration_converges_active_with_complete_postings`
  (Building→Sealing→Active over pre-existing records on both shards including the three absence
  shapes, equality multiplicity, label scoping, and post-Active rewrite swaps).
  Slice 4 is present on main since `39746f7b3` (2026-08-23) but its acceptance stays pending
  Plan 0285 validation: the planner's property-access extractor lowers a nested chain to
  the canonical dotted path so equality/range anchors select interned leaf identities
  (`crates/gql-planner/src/anchor.rs::extract_property_access`, shared by equality/range/
  intersection anchor selection and inline-WHERE collection), the node-pattern value lookup uses
  the same extractor (`match_plan/path/filters.rs::find_equality_value_in_where`), and Router seed
  probes resolve dotted names through the ordinary catalog/namespace path. Plan 0284 removed the
  slice-4 capability test `planner_stats.rs::active_catalog_projects_nested_leaf_as_indexed_and_
  range_indexed`; Plan 0285 re-adds it and runs its bounded contracts, including planner tests
  `planner_tests.rs::match_nested_leaf_*`, router contracts
  `seed.rs::vertex_{equality,range}_anchor_resolves_nested_leaf_property`, and the
  cross-shard GQL proof
  `crates/pocket-ic-tests/tests/router_gql_query.rs::federated_vertex_nested_leaf_index_match_equality_and_range`
  — success itself is the anchor proof because unseeded leading index scans are rejected on graph
  shards. Do not cite slice-4 code as terminal until Plan 0285 closes.
- **Severity:** P2 query capability
- **Owner:** Planner property-path resolution; decision owned by ADR 0073 slice 4
- **Observed behavior:** Nested leaf postings were maintained and backfilled end-to-end, but the
  planner did not select a nested index as an anchor (`COST BY v.stats.field` parsed;
  equality/range planning over `n.stats.score` did not bind to the interned leaf namespace).
- **Expected or needed behavior:** The planner must seed anchors and range/equality candidates
  against interned leaf identities under the same contract as every other indexed property.
- **Evidence:** `crates/router/src/planner_stats.rs` (membership projection carries
  `field_path`/`ancestor_property_id`), `design/index/property-index.md` "Nested record leaf
  domain", and the ADR 0073 implementation slices.
- **Impact:** Vertex nested-field range/equality planning cannot rely on the same index contract
  as edge inline fields until the anchor lands.
- **Next decision:** Implement ADR 0073 slice 4 (planner anchors) in a follow-up plan.

### GAP-2026-07-29-006 — Index activation convergence gate lacks production driver/E2E completion

- **Status:** Closed 2026-08-22
- **Severity:** P0 index correctness
- **Owner:** Router index catalog and backfill lifecycle
- **Observed behavior:** Router now persists hidden Preparing/Building/Sealing/Aborting lifecycle
  rows and derives planner membership only from Active rows. Graph owns exact canonical export
  scopes, graph-index owns resumable build state, the production Router cross-canister driver
  composes register/advance/seal/cleanup with unit coverage, and the CLI exact-replays one immutable
  artifact. Graph label gain/loss admission is now implemented in the Graph coordinator: exact
  `(label_id, property_id)` memberships are selected from the canonical label set, all affected
  namespaces are preflighted before mutation, Building emits the exact build envelope, and Sealing
  rejects before the canonical label change. The boundary is covered by
  `building_label_gain_emits_exact_property_insert`,
  `building_label_loss_emits_exact_property_remove_once`,
  `sealing_label_gain_rejects_before_any_mutation`,
  `sealing_label_loss_rejects_before_any_mutation`,
  `public_mutation_id_zero_label_wrappers_preserve_index_build_admission`, and
  `delete_internal_label_clear_does_not_emit_second_property_removal`. The cross-canister PocketIC
  proof is now complete (`crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`, three
  scenarios green at HEAD `3c75d2fb5`; inventory precondition `router_gql_query` 24 passed / 0
  failed): (a) one single-statement `CREATE INDEX` migration converges
  Preparing→Building→Sealing→Active over a two-shard federation with pre-existing vertex and edge
  sidecar data — equality/range reads stay correct throughout (label-scan fallback pre-Active,
  index-served post-Active, non-Person decoy excluded) and the router catalog projects
  Building→Sealing→Active in order; (b) while Building, an eligibility label gain is admitted and
  its pre-existing property value converges into postings, while Sealing an eligibility label
  loss rejects before canonical mutation (E2E mirror of the six Graph fence regressions); (c)
  upgrading all five federation canisters same-wasm mid-build preserves graph-index's registered
  build watermarks and resumes to Active with complete postings. Edge-INLINE backfill completeness
  stays owned by GAP-2026-07-29-001 and is deliberately not asserted. The "retryably fail-closed"
  posture resolved as documentation-only: no code-level gate exists — `apply_schema_migration`
  composes `real_index_migration_driver()` unconditionally and caller-driven exact replay (CLI
  `MAX_INDEX_APPLY_ROUNDS_PER_MIGRATION`) is the resumption contract.
- **Expected or needed behavior:** A newly declared index must remain pending until sidecar and
  INLINE backfill has completed for every attached shard, or the query planner must explicitly treat
  it as incomplete and fail closed. During Building, every label membership transition that changes
  eligibility for an already-indexed property must durably emit the exact per-physical-index build
  operation. During Sealing, that transition must reject before canonical state changes.
  Rebuild/drop must use the same lifecycle rule.
- **Evidence:** `crates/router/src/facade/store/backfill.rs`, `crates/router/src/planner_stats.rs`,
  `crates/graph/src/facade/store/labels.rs`, `crates/graph/src/index/catalog_context.rs`, and
  `design/index/property-index.md`'s router-owned active catalog description.
- **Impact:** A query can observe an index that is structurally present but incomplete, producing
  false negatives rather than merely falling back to a slower plan.
- **Confirmed implementation boundary:** Graph label mutation uses the ordinary label-pending path
  for label postings and the same index-build admission/fence owner as property transitions for
  exact label-scoped property eligibility. ADR 0059's touched-first BuildDml admission and
  pre-mutation Sealing rejection are covered by the six Graph regressions named above.
- **Resolution:** Catalog-epoch seal behavior and per-shard watermark convergence validated in
  PocketIC, including upgrade reopen; the migration endpoint's resumption contract is documented
  rather than gated. [ADR 0059](adr/0059-create-index-migration-backfill.md) remains the source of
  truth.

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
