# Graph scalar property preparation

Status: Partially Implemented — the six scalar contract tests, expanded PocketIC lifecycle,
Graph lint and three planned benchmark cases pass. Whole-Graph regressions and the PocketIC
dependency lint blocker prevent completion.
Anchor: 2026-09-18 02:03:28 UTC +0000.
Validation update: 2026-09-22 06:24:11 UTC +0000.
Initial repository: `95d7910878a5ee8af95af878f2b0df952edd70dc`.
Validation repository: `9a64049f47fc07542446d651d4b09421636b73c5`, with existing uncommitted changes.

## Scope and result

The actual Graph single-property vertex/edge SET and REMOVE paths now hold storage-owned preparation
across index admission. **The production backend remains the existing B-tree. No extent allocator
has been connected to Graph or activated.** This is a narrower integration step than proving the
[experimental extent-backed preparation](2026-09-18-property-store-prepared-change.md) at this fence.

The existing [shared property codec owner](../../crates/graph/src/facade/stable/vertex_properties.rs)
contains `PreparedPropertyChange`; both concrete property stores issue it through `prepare_change`.
The private fields bind the exact key, immutable new Value, storage-acquired previous Value and an
exclusive mutable map borrow. `PropertyKey` obtains the property ID from each actual key rather than
accepting a parallel ID or arbitrary caller-supplied previous snapshot.

In [the scalar facade](../../crates/graph/src/facade/store/properties.rs):

1. Resolve the canonical edge handle when applicable.
2. Pure storage-owned assignment validation runs before accessing the lazy property store, preserving
   invalid-SET rejection without initializing that store. Preparation revalidates at its own boundary
   and decodes the previous value. On an initialized owner, preparation performs no stable writes;
   dropping it cancels without compensating writes.
3. Graph derives the existing `PropertyValueChange`, performs pure index preflight and resolves the
   subject while retaining the exclusive property borrow.
4. When Building envelopes exist, reserve their sequences and append the actual durable intent.
5. Consume `commit(self) -> Option<Value>`; no recoverable error result follows the intent fence.
6. Release the primary-store borrow, then dispatch Active/TEXT work using the returned previous Value.

Absent REMOVE, including reserved property ID zero, remains write-free; stored NULL remains present.
SET to reserved zero remains an error. Bulk/initial low-level setters retain their ordinary insert
path: they do not acquire a new preparation or an extra get. Scalar validation delegates to the storage
owner both before lazy initialization and at preparation; no fallible setter is invoked after intent.
No reduction in validation/encoding calls is claimed.

## Limits of this boundary

- Regions 33/34, actual 8/14-byte keys, `StoredPropertyValue::V1`, memory policies, public APIs and wire
  shapes are unchanged. There is no migration, compatibility decoder, new dependency or journal.
- This is **not** a B-tree search cursor or prepared physical-body allocator. B-tree insert/remove
  still perform another search and internally decode their old-value return. Encoding still occurs
  inside insertion after preflight persistability validation. No early-body byte/work/heap bound or
  reduction in old-value decoding is claimed.
- After store initialization, the first durable effect within this scalar path is index sequence
  reservation/outbox append when applicable, otherwise the primary mutation. As in the prior facade, a first valid access can
  initialize an empty B-tree before a later index rejection; the no-write preparation contract does
  not include collection initialization. Invalid SET is checked before that lazy access. Unexpected allocation, codec or storage failures after it trap; native panic
  does not undo writes. Nothing establishes rollback across an await or earlier message segment.
- Current pure planning reads row/label/catalog/routing state, not the held property owner. Dispatch
  runs after releasing that owner. This is not a guarantee for arbitrary future callbacks or writes
  through aliased Memory handles.
- Multi-property preparation, initial/bulk composition, whole-record replacement, inline updates,
  sidecar relocation and deletion do not inherit a prepared-arena safety proof. Bulk edge SET remains
  unavailable. Candidate-backed Graph integration, resource budgets, layout/policy and activation
  decisions remain separate work.

## Contract tests

Two storage tests exercise cancellation and consuming commit through the concrete vertex/edge owners:

- `prepared_property_vertex_change_cancels_and_uses_latest_previous`: absence, NULL, long text,
  scalar replacement, REMOVE and repeated absent REMOVE; exact backing bytes unchanged on preparation
  and cancellation, unchanged guard, and reserved-ID behavior.
- `prepared_property_edge_change_preserves_parallel_slot_on_cancel_and_remove`: distinct adjacent
  slot, long old value, cancellation, NULL, removal and no-op removal with exact backing preservation.

[Graph index tests](../../crates/graph/src/index/canonical_export.rs) now protect:

- The existing absent Sealing rejection **and** seeded long old/guard values surviving SET/REMOVE.
- Existing exact 5→9 posting assertions, followed by 9→11 and REMOVE with exact subject, mutation IDs,
  contiguous sequences, removal/insertion bytes and no intent for absent REMOVE.
- A new actual canonical-edge fixture with two parallel occurrences, exact Building transitions,
  canonical bucket-label/slot identity, then seeded Sealing rejection preserving old/guard values,
  the complete existing outbox and both watermarks.

These assertions reject premature primary publication, stale old-value reuse, lost REMOVE postings,
retargeting a parallel occurrence and admission on absent removal. Rust ownership provides consumption
and exclusion; no new compile-fail probe is claimed for this slice.

The [existing PocketIC label-transition lifecycle](../../crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs)
also contains Building scalar 10→11→12, Sealing SET/REMOVE rejection, direct Active posting lookups
for the exact surviving vertex and absence of old/rejected postings, and Active REMOVE followed by
canonical NULL verification. Direct posting checks are intentional: a residual GQL predicate alone
could hide a stale posting. These expanded assertions **passed on 2026-09-22** in the normal selected
PocketIC test: one passed, five filtered, with no sequential-test substitution.

A final self-review found that moving all validation inside the TLS borrow would initialize an
unopened map before rejecting invalid SET. Pure `validate_property_assignment` now runs before that
borrow and is reused by preparation. `invalid_scalar_assignment_does_not_initialize_property_stores`
checks reserved-ID and unencodable-value rejection for both vertex and canonical-edge SET while
both property regions remain at zero logical pages. The first attempt was blocked before Graph
compilation by LARA code. The resumed test then exposed a fixture error: ordinary Graph edge insertion
opens the sidecar store even for an empty initial property list. The fixture now inserts real topology
through the existing LARA API and uses its returned canonical slot; neither zero-page nor exact-error
assertions were weakened. The corrected test and Graph lint passed on 2026-09-18, and the test passed
again in the 2026-09-22 full Graph run.

Fixture cost: four new native tests (two standalone maps and two Graph fixtures); two existing Graph
fixtures strengthened. PocketIC remains six tests overall, with one existing federation lifecycle
selected here: one PocketIC construction and one five-canister install per execution, no new fixture.
The initial runtime failed, a corrected build timed out, and a subsequent invocation was interrupted
by the user before completion. The 2026-09-22 invocation is the completed expanded lifecycle pass.

## Validation evidence

Evidence directory is recorded by `/tmp/gleaph-graph-prepared-current`:
`/var/folders/wr/xh_5mtts77zfg_zq1r7yzzj80000gn/T/gleaph-graph-prepared-hcy10rbf`.
Commands below run at repository root unless specified otherwise. Continuation logs are retained in
`validation-20260918T121546Z/`, `verification-20260922T051438Z/`, `execution-20260922T052030Z/`
and `completion-20260922T054912Z/` under that directory. The final continuation is also recorded by
`/tmp/gleaph-graph-prepared-completion-current`; its per-command JSON records include elapsed time,
terminal status and the reason for interruption.

| Gate | Result |
| --- | --- |
| `cargo test --offline --locked -p gleaph-graph --lib` | **Failed:** the 2026-09-22 verification ran1161 tests:1143 passed,18 failed (42.84s total,9.64s runtime). All six scalar preparation/index contract tests passed, including cold-store rejection. The earlier 2026-09-18 continuation had1148 passed/13 failed; all13 persist plus five batch/compaction/purge failures. The original pre-cold-correction1160-test pass remains historical, not current approval. |
| `cargo clippy --offline --locked -p gleaph-graph --lib --tests -- -D warnings` | **Passed:** `completion-20260922T054912Z/graph-clippy.log`,24.43s total, at the recorded validation HEAD. Existing manifest/dependency future-compatibility warnings are retained; no diagnostic suppression or dependency repair. |
| Scoped rustfmt and whitespace/content checks | Prior scoped checks passed; final documentation checks are retained separately. Whole-workspace formatting is not claimed. |
| `cargo test --offline --locked -p gleaph-pocket-ic-tests --test adr0059_index_build_lifecycle label_transition_fence_holds_at_e2e_building_and_sealing -- --nocapture` | **Passed:** `execution-20260922T052030Z/pocketic.log`,1 passed/5 filtered,209.30s total (3m03s build,24.34s runtime). The first failed runtime and later interrupted builds are not counted as passes. |
| `cargo clippy --offline --locked -p gleaph-pocket-ic-tests --test adr0059_index_build_lifecycle -- -D warnings` | **Incomplete, with a confirmed lint error:** `completion-20260922T054912Z/pocketic-clippy.log`,347.55s total. `ic-stable-text-postings/src/enc/frame.rs:183` fails `clippy::needless_range_loop`. Cargo then remained at `build failed, waiting for other jobs to finish...`; after300s without output the owned process group was interrupted (−15), not a normal101 completion. That source is byte-equal to HEAD. No fix, skip-build flag, warning allowance or alternate lint scope was applied. |
| From `crates/graph`: `canbench graph_exact_vertex_set` | **Passed:** `completion-20260922T054912Z/bench-vertex.log`, two cases,104.18s total including1m31s release build. The previous300s interrupted build remains historical. |
| From `crates/graph`: `canbench scalar_directed_128_sidecar_4_per_property` | **Passed:** `completion-20260922T054912Z/bench-edge.log`, one case,5.93s total including1.03s cached build. No `--persist` or benchmark YAML update. |

### Current B-tree Graph measurements

The normal `crates/graph/canbench.yml` target, default features plus `canbench`, and unchanged release
profile (fat LTO, one codegen unit) were used. Before resumption the prior log stopped at Graph
compilation and no completed Graph Wasm existed. No build profile, feature, target or wrapper fallback
was used to obtain these results. Compiler: Rust1.100.0-nightly (`574ff7d98`,2026-09-14), LLVM23.1.1;
canbench0.7.0. Measured Wasm:23,825,911 bytes,
SHA256 `1b63b56bd265962419468ce6e413a1d42232fef37cb91533bdde7f587270a029`, retained with the logs.

| Existing case | Instructions (displayed precision) | Heap growth (pages) | Stable growth (pages) |
| --- | ---: | ---: | ---: |
| `bench_graph_exact_vertex_set_1_decoy` |146.14K|0|8|
| `bench_graph_exact_vertex_set_1024_decoys` |225.16K|0|8|
| `bench_scalar_directed_128_sidecar_4_per_property` |41.36M|0|64|

The [vertex cases](../../crates/graph/src/bench/mod.rs) measure exact-target SET including plan decode
and completed canonical journal; fixture construction and Router/index RPCs are excluded. The
[edge case](../../crates/graph/src/bench/batch_placement.rs) includes128 adjacency insertions and four
scalar sidecar assignments per edge, not just one property setter. These are unchanged existing
fixtures, not new scope-specific performance oracles. None measures the actual Building/Sealing or
Active cross-canister dispatch workload. Timed stable growth is512KiB/4MiB for the complete respective
operations, not a per-property preparation cost; zero heap growth is not zero allocation or a peak bound.
Nested scopes must not be summed.

The saved Graph YAML has no vertex-case baseline. For the edge case it records40,930,349 instructions;
canbench reports+1.04% and labels this within its configured noise threshold. That threshold is not a
statistical confidence interval. The saved YAML's last commit is
`d851adbc780ef7101b3f3489c48246e53deac25c`; it is not a matched-source/toolchain A/B control for0364.
No property-preparation-only regression or improvement, full-Graph performance clearance, or extent
comparison follows from these three runs. The saved YAML, Cargo.lock, recorded input hashes, full
tracked diff and HEAD/index were unchanged across the gates.

The native verification observed unchanged source hashes and HEAD/index during execution. This
worktree's two LARA modifications were comments only; its source directory and target were not
symlinks to another worktree. The18 failures therefore reproduce on this checkout rather than proving
cross-worktree interference. A concrete LARA issue remains in the tiny arm of
`visit_edges_for_label_impl`: both orders reconstruct the edge and attach its label without attaching
`ordinal` as its slot; Graph's label-based point lookup relies on that metadata. Capacity/compaction
fixture assumptions also need separate review. No LARA fix or assertion relaxation was made here.

Other recorded failures: the first compile attempt stopped in concurrent LARA work at a missing
`LabeledSpanIter::Inline` match arm, before the new tests; this is not a successful test-first red.
A subsequent new-test compile needed the `GraphMutationExecutor` trait import. The first full native
run passed1159/failed1 because the edge test expected logical label221 rather than canonical bucket
label32989; the oracle now uses the exact returned canonical handle. An initial lint run encountered
an independently edited, temporarily unbalanced `traverse.rs`; no LARA repair was made by this slice.
Later native and Graph lint runs succeeded at the recorded checkpoint. The subsequent cold-store
regression attempt (`cold-rejection-red.log`) stopped at `labeled/graph/insert.rs:1117`: its call to
`relocate_labeled_leaf_physical_block` lacked the required `E: CsrEdgeTombstone` bound. This is the
historical compile blocker, not the result of the resumed validation. These failed transcripts
remain separate from later passes and the whole-Graph runtime failures.

No sequential-test substitution, GUI fallback, cold target directory, staging, deployment or commit
was used. Whole-Graph runtime failures and the PocketIC dependency lint blocker prevent plan closure;
the scalar, E2E, Graph lint and benchmark passes do not replace those gates. The E2E run plus timed-out
benchmark consumed about509s in `execution-20260922T052030Z/`. The later authorized continuation used
482.10s of long-command observation across four commands; no lint retry or full native/E2E rerun
followed. No full313-case remeasurement, final benchmark YAML update or lifecycle upgrade run is claimed.

## Ownership and review

Same-assistant review, not independent/security approval:

- **Encapsulation:** callers cannot substitute the stored previous Value, key, or new input at commit.
  The shared type lives with the codec/error owner already used by both stores, not a new subsystem.
- **Separation:** storage validates/acquires/mutates; Graph owns index planning, subjects, intent and
  dispatch. No LARA, Router, planner or index-canister policy moved into storage.
- **Invariants:** one synchronous exclusive borrow; pure errors precede the intent fence; consuming
  commit has no `Result`; post-commit dispatch holds no preparation reference.
- **Consistency:** existing `PropertyValueChange` and build outbox remain the semantic/durable owners.
  Ordinary direct operations, codecs and unrelated batch paths are not replaced or shadowed.
- **Fitness/complexity:** one internal capability and one key-metadata trait serve the two existing
  concrete keys, plus a pure assignment-admission helper needed before lazy store initialization.
  No new mode flag, framework, cache, schema or module. New APIs have at most three
  meaningful parameters; the pre-existing five-parameter scalar entry points are unchanged. Small
  repeated scalar orchestration stays explicit rather than adding a callback/mode framework.

Historical pre-fixture-correction Lizard1.24.0 measurement, explicit scope of the five changed Rust
files against their original clean HEAD baseline:
205→224 detected functions, NLOC4893→5422, CCN579→632. Directly touched detection is6→26 functions,
CCN31→85, maximum8→9. These totals include the additional contract fixtures and unencodable test
extension. The four scalar coordinator CCNs are8→9,5→8,6→7,6→9: fallible preparation and dispatch
outside the borrow add explicit control flow. Rust trait/nested parsing is imperfect; no complexity
reduction or performance improvement is inferred. Keep the explicit owner/fence phases rather than
move dispatch under the borrow or remove validation to lower a metric. Raw CSVs are retained.

Review disposition: **hold for final validation**, not independent approval or production readiness.

Historical experiment sources/results and closed plans are preserved. Source hashes and the exact
owned diff are retained with the evidence, along with named concurrent changes. The baseline was
supplemented for the initially clean ADR0059 and PocketIC test from pinned HEAD (confirmed clean by
`status.before`); no worktree source was restored. The source study is marked as a0362 checkpoint,
with the B-tree follow-up distinguished from still-unimplemented candidate-backed Graph integration.
