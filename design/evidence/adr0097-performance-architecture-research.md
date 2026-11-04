# ADR0097 performance architecture research (whole map-edit / FreeSpan execution)

Status: **Experimental — research and bounded implementation checkpoints, not ADR0097 acceptance.**
Sections0–15 preserve their named source epochs: their current-source wording, measurements and
hypotheses apply to those checkpoints, not the live implementation. The latest retained map source
is `47796acb` (Plan0362), described in [section16](#16-reserved-resize-retained).
Original acceptance remains4/6; replacement and tail remain open, with no relaxed ceilings.
[Section12](#12-structural-options-beyond-local-map-tuning) compares broader storage-format,
allocator-operation and index alternatives on the0358 source.
[Section13](#13-order-preserving-rekey-rejected) records option A's rejection and full restoration;
[section14](#14-compact-page-header-retained) records option B's retained layout. Sections15 and16
retain shared scratch append and resize improvements without changing that layout. Option C remains
a separate selection-policy decision.
[Section17](#17-paired-edit-start-metadata-read-rejected) records the paired-header-read trial's
rejection and full restoration. No part of that candidate changes the retained layout or source.
[Section18](#18-shared-edit-pipeline-static-survey) inspects that retained source and frozen Wasm;
it selects no implementation candidate and adds no performance measurement.

## 0. Gates, anchors, non-claims

Measured R (frozen checkpoint `design/evidence/adr0097-demotion-design.md` lines 63-85 plus the
original scalar/allocator command stdout under `design/evidence/sources/`; no R CSV exists and
none may be reconstructed):

| Gate | R | original ceiling | R/ceiling | reduction required from R |
| --- | ---: | ---: | ---: | ---: |
| scalar_missing_64 | 128296 | 85672 | 1.4975 | 33.22% |
| scalar_replace_64 | 149000 | 77576 | 1.9207 | 47.94% |
| scalar_tail_pairs_64 | 561354 | 298171 | 1.8827 | 46.88% |
| scalar_split_directory | 69444 | 30930 | 2.2452 | 55.46% |
| LARA total | 2147387 | 1527576 | 1.4057 | 28.86% |
| LARA named | 2146426 | 1526615 | 1.4060 | 28.88% |

- All six ceilings remain binding FAIL. Current source e545a25c… / Wasm 835d503b… has **no IC
  measurement**. Q is not a comparator for the candidate.
- Prices are whole-delta history, never per-copy/per-check attribution: caller-owned image
  correction moved original-blocked missing 242856→158312 with unchanged native traffic;
  pre-cover work C→D moved split 150572→70627; R moved split 70497→69444 (−1053). Current work
  must not reimplement those fixes. The M instrumentation epoch perturbed timings and cannot be
  subtracted from anything.
- Non-attribution: instruction totals are canbench's monotonic instruction counter; heap increase
  and stable-memory increase are **separate metrics** and are never added to instructions
  (https://github.com/dfinity/canbench — README reports instructions and memory changes, not
  timing/averaging; dirty-page reporting is future work). Native sizes, native traffic, CPU-cache
  results (e.g. CSB+) and rounded K console output assign no IC cost. Per-read IC prices: none
  claimed. Official performance counter semantics:
  https://docs.internetcomputer.org/references/ic-interface-spec/canister-interface/#system-api-performance-counter
  (deterministic monotonic approximation of executed work; stable64 read/write copy bounded
  ranges and trap out-of-bounds). Local runtime epoch: published canbench0.7.0, client/server
  PocketIC 13; Gleaph E2E client 15 is separate; no newest-source numeric prices applied
  retroactively.
- Binding invariants: D1/D2, exact error precedence including DirectoryFull before the directory
  scan, full-page extent checks, scalar error projections, ordinals/sidecars/tombstones/
  multiedges, nonpublication and exclusive backing, single prepared/scalar owner, no early write.
  Trap rollback never licenses publishing earlier or converting recoverable failures.

## 1. Current map-edit execution model (source `crates/ic-stable-paged-ordered-map/src/lib.rs`, e545)

Flow: public `prepare_edits` (1353, `&[Edit]` → opaque owner-bound `PreparedEdits`) and
`scalar_edit` (1360, K=1) both go through `prepare_image` (1369) → `EditPlan::apply_edit`
(954; body extends past 1061) per edit → `admit_image` (1429, **at most one grow per plan**) →
`publish_image` (1451, R writes of B staged bytes after all checks). K means edits **in one
plan**: each scalar call has K=1; `scalar_tail_pairs_64` is 128 calls; `scalar_split_directory`
is 1 call. The batch surface already exists publicly; FreeSpan/A0/A1 wiring remains
unimplemented/unapproved — a batch diagnostic does not create a new public API, and batch
savings cannot substitute for any scalar gate.

Per-call fixed path: `EditImage`/`EditPlan::new` (595: memory size + page-count read), scratch
reserve, then per-edit prospective read validation: `dir_len` (625), `directory` (633),
`locate_directory` (O(log D) image probes), `page` (687, count guard + header field validation),
`search_page` (O(log P)). This common plan/setup/read-validation path is shared by all four scalar
scopes and is **priority (1)**. Static presence of work is not instruction attribution; whether
fixed setup or per-op validation dominates is a hypothesis that would need controlled observation.

Structural byte movement, bounded correctly: `EditImage::cover` (496) has a **contained** path
(the early return when a write fits an already-staged range) costing O(log R_i + payload); an
**extending** emission moves trailing packed bytes and shifts/rebases descriptors inside `cover`
costing O(B_i + R_i + log R_i) per emission i. Total staging work is the **sum over emissions** —
not superlinear in K at fixed B. The split path pre-covers the right destination and the source
entry span (`split_page`'s two `image.cover` calls) and reuses position, which avoids per-entry
extending normalization of those entry regions — but NOT all repeated payload movement: later
metadata writes (headers, links, directory entry/length, further range extensions) can still
extend the packed region and relocate packed bytes and descriptors. Directory suffix moves
already use one block `cover` + `copy_within` (`insert_directory` 642, `remove_directory` 659).
Published R writes/B bytes are canonical publication, not staging work.

What the present fixture actually exercises (static path presence ≠ measured cost):

- `scalar_missing_64`: absent-key removes make **zero dirty writes and zero cover calls**
  (no-op remove arm of `apply_edit`), so normalization/shift/byte-movement cannot explain the
  missing gap at all. Missing is the **control** for common prepare/read cost.
- `scalar_tail_pairs_64`: tail inserts run the entry-shift loop with **zero iterations**
  (pos == len) and the fixed fixture's tail removals also shift nothing; bulk shifts therefore
  cannot directly improve tail, replace or missing.
- `scalar_replace_64`: replacement creates a fresh empty image, so the scope is **8-byte
  value-only staging/publication; its first cover need not take the contained-range fast path**.
- `scalar_split_directory`: D=2 in the fixed fixture — the O(D) split directory re-scan inside
  `split_page` (duplicating the `locate_directory` result already computed by `apply_edit`) and
  flat directory suffix shifting are **scale risks, not evidence they dominate the present gap**.
- P ≤ 64 (page occupancy); n=128 fixtures are not single-page occupancy controls. The map
  free-page list (allocate_page 717) is a distinct mechanism from FreeSpan size bins.
  `cfg(test)` counters do not exist in production canbench runs, and `observe_image` (1470)
  does not count relocation bytes.

## 2. FreeSpan execution model (`crates/ic-stable-lara/src/lara/edge/free_span.rs`)

- `by_start` is a **flat directory of fixed-capacity sorted pages**, not a paged B-tree.
- `pick_span_in_bin` (931): bounded scan `MAX_SCAN = 8` (943) approximates best-fit inside the
  start bin; if nothing fits within those 8 records it continues as **first-fit over the rest of
  the start-bin list**, which can be O(F) in the start-bin tail — the source comment states a
  usable span must not be skipped and higher bins fit on their first member. Selection therefore
  approximates best fit, not global exact best fit. This preserves the usable-span-not-skipped
  and exact-prefix behavior; an occupancy bitmap alone can skip empty bins but cannot remove the
  partial start-bin scan.
- `release` (498) uses two lookup paths (`predecessor_or_equal` and `next_span` (924); the
  comment notes the previous shape needed four). `take_best_fit` (644) splits via bin unlink,
  `by_start` remove+insert, relink, max/summary updates. `read_record` (1152) loads six fields;
  `write_record` (1164) already batches the 48-byte record (`RECORD_STRIDE = 48`, line 57) in
  one buffer. No per-operation write-count totals are claimed here.
- Warm/cold lazy max-heap work and occasional rebuilds (repeated `by_start` successor steps plus
  heap pushes) may depend on F; **flat F measurements would not falsify constant per-operation
  index fan-out**. Population/reopen cold behavior is separate from steady-state cycles.
- Fixed LARA gate fixture: `bench_r_fs_tkbfs_1024`
  (`crates/ic-stable-lara/src/lara/edge/free_span/bench.rs`, `bench_take_best_fit_split` line 33):
  populate 1024 spans **outside** `bench_fn`, then 16 `take_best_fit` + `restore_allocated_prefix`
  cycles inside the scope; both gate rows are total/named of this same fixture (difference 961).
  Ordinary split/rejoin costs two map scalar edits for the take and two for the replacement of the
  moved start; the whole-span case differs. fallback 256/4096, reopen and largest-bin benchmarks exist (bench.rs 135-278)
  but were **not measured in the frozen-R gate campaign**; their source epoch must never be
  inferred from the current broad repository CSV (`crates/ic-stable-lara/canbench_results.csv`).

## 3. What the six gates do not cover

Batch plans (K>1 in one image), large directories (D ≫ 2), fragmentation at high F, grow-heavy
admission, tombstone/sidecar/ownership paths, full-A0/A1 prepared interfaces, whole-graph/router
calls, fallback/reopen/largest FreeSpan shapes, and the current e545 source itself. No gate
aggregates; no Q comparator; no new campaign implied.

## 4. External primary sources (fetched/read by coordinator; lessons and non-permissions)

- **A. RocksDB Write-Batch-With-Index wiki**
  https://github.com/facebook/rocksdb/wiki/Write-Batch-With-Index — append-only WriteBatch plus
  skip-list key→offset index; read-through checks batch first, then DB; `overwrite_key` controls
  visible index entries, not dedup of the append log (every operation remains). Lesson: pending
  lookup and final physical packing need not be the same representation. **Not** permission to
  import RocksDB/skip-list or a general journal: raw append history can violate dirty-byte
  scratch bounds. Any late compaction representation must preserve ordered priors/errors, bound
  scratch, and finish encode/reserve before ready — prefer the existing EditImage owner and
  operation/span-sized updates. Not proof of Gleaph speed.
- **B. TLSF original paper** (mirror of authored paper, DOI 10.1002/spe.858)
  https://www.wide-dot.com/tlsf/paper/spe_2008.pdf §§4.2-4.3 (pp.11-13) — two-level size
  classes/bitmaps with **incomplete** good-fit search:1524B available cannot satisfy a1512B
  request because the search starts at the1536 class; the paper discusses round-up split to
  reduce that structural waste; mattconte's implementation
  https://raw.githubusercontent.com/mattconte/tlsf/master/tlsf.c rounds the search class and uses
  bitmaps in `search_suitable_block`. Therefore no drop-in O(1) allocator claim: it would not
  automatically preserve our usable-span-not-skipped + exact-prefix behavior. Ordered size
  index/finer classes are separate selection/maintenance/layout tradeoffs needing decisions and
  fragmentation evidence. TLSF's in-band boundary tags are not our edge-slab metadata.
- **C. SQLite official** https://sqlite.org/fileformat.html#b_tree_pages (cell-pointer array of
  2-byte offsets in logical key order; payloads elsewhere; freeblocks coalesced/defragmented) and
  https://sqlite.org/btreemodule.html#balance_siblings (redistribution across up to two siblings).
  Lesson: slot indirection reduces moved payload on insertion but adds indirection and
  free-space management; our fixed 16-byte entries and tiny P≤64 may not benefit. That is a
  persisted-layout redesign (fresh state + ADR), not a fix for unchanged value/missing/tail, and
  distinct from the parked directory-slot reuse idea (which stays parked and out of recommended
  work).
- **D. DFINITY ic-stable-structures 0.7.2 versioned source**
  https://docs.rs/ic-stable-structures/0.7.2/src/ic_stable_structures/btreemap.rs.html
  lines 493-649 / 1294-1309 — a multi-level B-tree split changes sibling/child/parent and saves
  nodes/header as it proceeds (in 0.7.2, `btreemap::load_node` invokes `Node::load`; split saves
  sibling/child/parent directly — no claim about a node cache on latest main). A tree directory
  is a **potential** answer to O(D) flat suffix shifting at large D; a plain StableBTreeMap is
  **not** a drop-in nonpublishing prepare/discard protocol. The comparison fixture already exists
  (`crates/ic-stable-paged-ordered-map/src/bench.rs`: predecessor/successor and mixed 1024/4096);
  source existence is not performance evidence.
- **E. LMDB author's design** https://www.openldap.org/pub/hyc/20111010LDAPCon-MDB.html §§3.2,
  4.1, 4.2 — COW, alternating meta roots, freeDB, oldest-reader page reuse. Useful contrast only:
  it copies pages/paths and adds version/reclamation state our exclusive prepare borrow does not
  require. Do not import COW/MVCC as free atomicity or free performance, and do not assume page
  release shrinks IC stable backing. Large redesign; not warranted by these tiny gates.
- **F. IC/canbench official** — performance counter semantics as cited in §0; canbench README
  https://raw.githubusercontent.com/dfinity/canbench/main/README.md reports instructions and
  memory changes rather than timing/averaging, with dirty-page reporting future.

## 5. Priorities, alternatives, and their class

Priority order for future study (research direction, not approved work):

1. **Whole common prepare/read-validation cost** — missing is the control (zero staging there).
   First consider a contract-preserving **internal redesign of shared editing granularity and
   phase responsibilities inside EditPlan/EditImage**, not another helper branch.
2. **Affected-span transformations within the same engine** instead of per-entry normalizer
   round-trips — relevant only to split/interior shapes (tail shifts run zero iterations;
   bulk shifts cannot directly improve replacement/missing/tail).
3. **FreeSpan operation-level grouping / repeated index work** — separately gated A0 integration
   (owner wiring unimplemented), not part of the six-gate slice.

**Macro hypothesis (read-only derivation; NOT implementation acceptance): the operation boundary may matter more than the allocator algorithm.** On valid disjoint free spans, splitting `[p,q)` at `0<m<q-p` changes the indexed start `p→p+m` while retaining the `SpanId`; no other free start lies in `[p,q)`, so relative key order need not change, and correct restoration of the allocated prefix reverses it. Yet `take_best_fit` (644) and `replace_existing_span` (1045) each express a changed start as `by_start.remove(old)` + `insert(new)` — non-tail map calls shift suffixes in both directions — while the fixed scalar tail gate exercises zero such shifts. Designs to **compare**: (a) the existing ordered batch to amortize preparation (does not remove all shifts); (b) an explicit ORDER-PRESERVING REKEY semantic operation owned and validated by the SAME map engine, potentially only key/minimum writes. (b) is a NEW API/enum/error/admission design decision — not permission to delete/reorder/cancel current Remove/Insert intents and not an assumption that old intermediate page/error semantics stay identical; the generic map must validate its own ordering and geometry, and FreeSpan must retain non-overlap/identity/bin/summary consistency. User decision plus full bounds/errors/atomicity/scratch/ordinal review and measurements precede any implementation. No assumed speedup; no claim it addresses the four scalar gates. Possible value: avoiding redundant physical shifts in LARA split/restore's four-map-edit ordinary cycle; its share of the 619811 regression is UNKNOWN.

**Symmetric existing owners to reconcile before any future patch:** `take_best_fit` carries an inline unlink/index-remove+insert/relink/max/summary replacement, while `take_prefix_at` delegates to `replace_existing_span` (also used by `release` and `restore_allocated_prefix`) — the same span-replacement rule under different lookup/error entry conditions. A future change must first unify this rule at FreeSpan's existing owner, without deleting diagnostic checks or adding a second editor. No refactor now; `write_active_record` (991) and `relink_active_record` (1022) also duplicate bin-prepend mechanics — a code-duplication fact only, not IC cost attribution.

Options, not selections: late write-set normalization is available as an option with scratch and
read-your-writes tradeoffs, not a chosen answer. Conditional scale alternatives needing explicit
user decisions, persisted-layout/fresh-state (ADR) status, and their own evidence: large-D tree
directory (D), slotted pages (C), allocator size indexing (B). Not warranted by the fixed tiny
gates: COW/MVCC, blanket caches; LSM is **unexplored**, not source-supported. Rejected/parked
stay closed: F/G branch placements, directory-slot reuse, key-only probing.

Sizing classes for any future proposal: **minimum** = internal EditPlan/EditImage phase/
granularity changes (contract-preserving); **medium** = owner-level integration such as FreeSpan
operation grouping behind the existing public surface (A0 decision); **large** = persisted
redesigns (tree directory, slotted pages, allocator indexing, COW) requiring user decision and
fresh state. This tiering is not an approved implementation plan.

Boundary/encapsulation paragraph: all of the above stays behind one editor — `EditPlan`/`EditImage`
own staged bytes, admission and the single publish; no second fast-path editor, no copied domain
rules (DirectoryFull-before-scan, full-page extent, error projections stay single-owned), no early
write, no new state outside the owning struct; the five review axes (encapsulation, separation of
concerns, invariants, consistency, fitness for purpose) are satisfied only by changes that keep
those properties, and any change that moves a rule to a second site is rejected regardless of
measured effect.

## 6. Next design question and smallest observation

**Concrete next design question:** can the shared per-call prepare path (`EditPlan::new`,
dir_len/locate/page/search validation reads, admission check, publish fan-out) be restructured at
the granularity/phase level inside EditPlan/EditImage so one owner-level pass serves each plan —
preserving per-edit prior values, exact error order, and the single-publish boundary?

**NET GROUPING diagnostic — implemented and measured; production adoption not approved.**
The bounded diagnostic in plan0344 appends `bench_paged_missing_grouping_97` to the
existing benchmark module without changing its original benches or production
`lib.rs` (`515b5e9e…`). It holds the operation count at 64 on the existing 97-key
fixture: (a) 64 scalar absent-key removes and (b) one `prepare_edits` batch of the
same 64 intents followed by **publish**. Both arms use the same map, with exact
contents/geometry checked before measurement and unchanged backing size/bytes
checked after each unmeasured preflight and after measurement. The batch preflight
also publishes. Inputs, fixed output arrays, snapshots and assertions are outside
`bench_fn`; outputs start with `Some(u64::MAX)` so skipped result writes cannot
masquerade as successful `None` results.

The sibling scopes `scalar_calls_64` and `prepared_batch_64` consume all 64 results
with per-result black-box barriers into fixed arrays; batch publication and scratch
destruction occur inside its scope. Scalar keys cross a barrier per call, whereas
the batch input slice is obscured once. The batch also needs dynamic internal
storage for 64 priors, while scalar calls keep theirs inline. Thus the comparison
measures **net grouping including those API/loop/storage differences**, not an
isolated constructor or per-call setup price. Its new scalar scope has different
output handling from historical `scalar_missing_64` and is not a replacement
measurement for that gate. Report matched S, B, S−B and `100*(S−B)/S`, with memory
counters separately; the combined total is diagnostic only.

This single fixed-cardinality comparison does not authorize production batching,
FreeSpan integration, a new editor, or a sweep. If net gain is absent, grouping
alone lacks support and the common validation path still needs its own scrutiny.
The six original ceilings remain independent binding gates; their failed historical
measurements are not waived or relabeled by this diagnostic. The historical 600/120
budgets remain unproved. No 2× improvement is claimed or implied.

### Source-bound result

Evidence root: `$GLEAPH_EVIDENCE/adr0097/ng-direct-s555`.
Production `lib.rs` remains `515b5e9ed92be170b2d96bf98f3d3f56219c757897a83867c81ee4164bba228d`;
benchmark source is `11255B/e63a23bdc96c04165f9efdfee77ce869819b10e3950e9216a9cc07a7afaffc31`.
The release/canbench Wasm is `728913B/830fc6715b1c997376a1b1c999b7b8cfb4602568bd15d22b5fcda2aa9ebb9b53`,
bound to the successful Cargo JSON record in `wasm-binding.json`. Feature-enabled
host clippy (`--lib --tests -- -D warnings`), the Wasm build and the single filtered
canbench invocation all passed. No default-feature unit/doc rerun, LARA build or
original-gate remeasurement was performed.

| Diagnostic scope (64 operations each) | Instructions | Heap increase (pages) | Stable-memory increase (pages) |
| --- | ---: | ---: | ---: |
| Scalar calls, S | 103300 | 0 | 0 |
| Prepared batch plus publish, B | 83201 | 0 | 0 |

Net saving: **20099 instructions, 19.4569% of S**. Combined total: 188439 instructions
(diagnostic only, not an acceptance gate). `map/results.csv` is the row source;
`measurement.json` binds its hash and the validated three-row schema. Runtime success
includes both arms' unmeasured checks and the measured outputs' postconditions:
64 `None` results per arm, unchanged canonical backing bytes/size and exact valid
97-key content. The original benchmark prefix and production code are unchanged.

This establishes a net benefit from grouping in this fixture, including the extra
batch result storage. It does **not** identify which setup/validation component
accounts for the saving, predict a scalar optimization's gain, or demonstrate any
of the six ceilings. A follow-up should identify removable common work inside the
existing owner before choosing a production change; batching itself remains a
separate decision. Failed `ng-src-s545` and `ng-src-s553` attempts and all earlier
failed gates/fidelity findings remain unchanged.

## 7. Replacement phase scopes on the metadata-first checkpoint

**Status: diagnostic measured; temporary probes removed; no optimization adopted.**
The retained source is `073473e40d18efe5d834f005e08bbb2a902d7e7903142aa69d8b137fbda1b234`,
committed in `4f2afd9df`. Its original replacement gate remains116872 against77576;
this diagnostic does not replace that fixture, rerun the six gates, or change acceptance.

### Measurement contract

A temporary `bench_paged_replace_scopes_97` uses the existing97-key/two-page seed and the actual
public `insert` facade. It alternates key64 between two distinct values for64 calls, with inputs
and sentinel-initialized output arrays allocated before measurement. Preflight checks exact pairs,
page/directory geometry, replace/restore priors and the exact changed8-byte destination. After the
measured loop, all64 priors, complete backing bytes/size and valid contents must match. A no-op
setter fails preflight; skipped measured calls or output stores fail the prior-array check.
There is one map/Memory constructor per diagnostic invocation, no new unit/PocketIC fixture.

The same diagnostic source is built first with the pristine map, then with eight temporary
`bench_scope` probes in the existing owners. `diag_replace_64` encloses only the replacement loop;
a sibling loop records64 empty scopes as a reference. Fixture construction, assertions and the
empty-scope loop are outside that replacement scope. The benchmark's fixed input/output arrays
differ from the original scalar gate's loop/output handling, so its control is not a new gate value.

The instrumented path observes image construction, plan/result reservation, lookup, matched-key
staging, cover, prior storage, admission and publication. Each internal scope records exactly64
calls, and the outer replacement scope records1. canbench-rs0.7.0 resets preflight measurements at
`bench_fn`, aggregates by scope name, and retains inclusive measurements for different nested names.
The cover scope is inside replacement staging; these two numbers must not be added together.

### Source-bound results

All listed leaf/reference rows cover64 calls. Counts include measurement overhead.

| Scope | Instructions | Instructions/call | Boundary |
| --- | ---: | ---: | --- |
| `diag_lookup` | 54336 | 849 | Directory selection, page validation and key search |
| `diag_cover` | 36672 | 573 | Dirty-range normalization, nested in replacement staging |
| `diag_prepare_setup` | 25728 | 402 | Plan construction and result reservation |
| `diag_publish` | 25344 | 396 | Canonical dirty-range publication |
| `diag_record_result` | 21440 | 335 | Previous-value projection and storage |
| `diag_admit` | 18496 | 289 | Final backing-admission check |
| `diag_image_init` | 17664 | 276 | Image-construction source boundary |
| `diag_empty_scope` | 17664 | 276 | Empty reference; identical in both builds |

The inclusive `diag_replace_stage` records136021 instructions over64 calls. It contains the cover
probe and its bookkeeping, so it is not an exclusive update cost. The outer replacement scope is
117150 without internal probes and1060185 with them, a9.0498× increase. Whole diagnostic totals,
which also include calibration, are190076 and1161079. All reported heap/stable page increases are0.
The large instruction increase is observer/codegen overhead, not a production regression.

Subtracting the empty reference gives descriptive differences of573/call for lookup and297/call
for cover, but these are **not calibrated production costs or predicted savings**. Probe calls can
change codegen, and surrounding scopes include child registration. Image initialization matching
the empty reference does not establish zero initialization cost: source boundaries do not prevent
compiler movement. No exclusive-cost pie chart or claim that the scopes sum to the uninstrumented
call is justified.

### Diagnosis and next decision

Lookup is the largest measured leaf. Cover is material, but the evidence does not support choosing
it as the dominant cost before inspecting selection/validation/probing. The next bounded question
is which part of `EditPlan::lookup` and its backing reads can avoid repeated work while retaining
all selected-page checks and exact corruption/error precedence. Cover remains a second candidate;
there is no measured optimization gain yet. A finer lookup measurement or one evidence-backed
change must use the existing owner, not a separate scalar editor or weakened validation.

Both variants passed Wasm-target, canbench-enabled clippy, release builds and their single filtered
IC run, including all runtime pre/postconditions. Control/profile have exactly3/11 CSV rows with
expected names and calls; results were not persisted. Both Rust source files were then restored
byte-for-byte, including all28 existing tests and the original benchmark prefix. The prior28-unit/
5-doctest validation is reused, not rerun. No LARA build, original-gate sweep or production change
was performed, and ADR0088's current contract remains unchanged.

Evidence: `$GLEAPH_EVIDENCE/adr0097/replace-scopes-0354/`.
`control/` and `profile/` contain source snapshots, successful Cargo records, command streams and
CSVs; `wasm/` contains frozen outputs copied before later builds. Control Wasm756405B/SHA256
`9b98b163061dc6f4b8ba6772eff55722db6d5860180e5f507fb2c0a2ee1cfecd`; profile757436B/SHA256
`acafccc7b5fd32f5427a7d3bb9bfd5a3b7a1422b75e8a0d5d2d9c15b2b650634`.
`measurement.json` records the comparison and limits; `restoration.json` proves exact restoration.

## 8. Lookup subphases and bounded directory-read candidate

**Status: diagnostic measured; temporary probes removed. Pair-loading candidate tested and rejected in [section9](#9-adjacent-directory-pair-experiment-rejected).**
Retained map source and commit remain `073473e4` / `4f2afd9df`. The exact section7 replacement-only
fixture is reused, including every input barrier, sentinel prior and pre/postcondition. Three sibling
scopes replace the earlier eight probes; only `EditPlan::lookup` is instrumented. The source-bound
uninstrumented control is reused, not rebuilt or rerun. Each phase and empty reference records64
calls; outer replacement records1. No preflight samples leak into the measured rows.

| Scope | Instructions /64 calls | Per call | Difference from empty reference /call |
| --- | ---: | ---: | ---: |
| Selected-page checks and minimum agreement | 36288 | 567 | 291 |
| Directory selection | 32128 | 502 | 226 |
| In-page key search | 24576 | 384 | 108 |
| Empty reference | 17664 | 276 | 0 |

These counters include measurement overhead; the differences are descriptive, not calibrated
production prices or predicted savings. The outer replacement scope is428072 versus the reused
117150 control (3.6541× with instrumentation). Whole diagnostic total501671 includes calibration.
All heap/stable page increases are0; this does not assert zero heap allocations. Do not sum these
children against section7's whole-lookup number: the two builds have different instrumentation and
codegen. This remains a fixed-key diagnostic, not a replacement for any of the six original gates.

### Read dependencies and next candidate

For this validated two-entry directory and key64, source inspection gives the following read path:

| Phase | Source-derived backing reads | Bytes |
| --- | --- | ---: |
| Directory | Length at48; entry1 at80, then entry0 at64 | 8+16+16 |
| Page | Page1 header at131136; first key at131168 | 32+8 |
| Search | Midpoint32 key at131680; immediate equality | 8 |

These are six distinct addresses and88 bytes, not a dynamic traffic-counter measurement. Initial
page-count loading and the prior-value read occur outside lookup. There is no same-address reread
inside this particular lookup. Header geometry, count/link bounds, active-page checks and agreement
between the directory minimum and first key enforce different invariants and must remain intact.
The largest phase is therefore not justification for deleting checks or trusting the directory
minimum without reading the selected page. A40-byte header-plus-first-key read would exceed the
existing scalar32-byte maximum-read assertion and is not a drop-in optimization under that contract.

A narrower candidate is **lookup-local adjacent directory-pair loading**. Entries0 and1 occupy32
contiguous bytes; the observed midpoint sequence reads them separately. One32-byte read could
supply both while retaining the same directory search/decoding and all selected-page checks.
For this fixture that would remove one backing call without changing the directory bytes read.
The existing lookup owns the probe and temporary entry buffer; no cross-edit cache or second editor
is needed. Binary-search probes and the insert-below-minimum fallback must share the same read
owner. Any candidate must preserve one-entry/odd-tail bounds, staged-directory visibility, exact
errors and the32-byte maximum, and must not prefetch outside the logical directory.

This is a proposed experiment, not measured gain. Pair buffering adds copying/branches and may read
unused neighboring bytes on larger directories; all original map/LARA controls must decide its
tradeoff. Cover's first-write normalization remains an alternative. No further profiling or guard
removal is implied as a prerequisite; the next step is one bounded implementation experiment.

### Validation and evidence

Wasm-target canbench-feature clippy, release build and the single filtered IC diagnostic passed,
including the unchanged exact-result/backing/geometry assertions. The CSV has exactly6 expected
rows/call counts; no results.yml was persisted. Removing only three guarded scope declarations and
three guarded drops reproduces the original lib. Both lib.rs and bench.rs were restored byte-for-byte;
all28 tests, original benchmarks, public/storage contracts and prior acceptance remain unchanged.
Native28-unit/5-doctest evidence is reused, not rerun. No LARA build, gate sweep or optimization ran.

Evidence: `$GLEAPH_EVIDENCE/adr0097/lookup-scopes-0355/`.
`profile/` binds source, Cargo output, raw streams and CSV; frozen Wasm756917B/SHA256
`54de75fec8924aafde8ca07b5f23240682185aaf3ee1d8dc2e999e999d2ffec3`.
`measurement.json` distinguishes actual counters from source-derived read accounting and the
unmeasured pair-loading proposal. `restoration.json` records exact source/status preservation.

## 9. Adjacent directory-pair experiment: rejected

**Status: measured and rejected; source and tests restored exactly to retained `073473e4`.**
Plan0356 tested section8's proposal in the existing `EditPlan::lookup`. A single local closure served
binary search and the insert-below-minimum fallback, reading an aligned pair into32B and limiting
an odd tail to16B. The buffer expired with each lookup; both backing/prospective routes used the
existing probe. Search/validation, error order, staging, admission and publication were unchanged.
No new helper, type, API, dependency or editor was introduced.

The original uninstrumented fixtures, not the replacement-only diagnostic, produced:

| Metric | Retained0353 | Pair candidate | Difference | Change | Original ceiling | Gate |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Missing64 | 89896 | 92712 | +2816 | +3.1325% | 85672 | FAIL |
| Replace64 | 116872 | 118024 | +1152 | +0.9857% | 77576 | FAIL |
| Split/directory | 29581 | 29597 | +16 | +0.0541% | 30930 | PASS |
| Tail pairs64 | 406346 | 412362 | +6016 | +1.4805% | 298171 | FAIL |
| LARA total | 841909 | 846873 | +4964 | +0.5896% | 1527576 | PASS |
| LARA named | 840948 | 845912 | +4964 | +0.5903% | 1526615 | PASS |

Replacement fails the improvement hypothesis, and every control increases. There is no benefit to
justify the extra buffer/validity state, so the whole candidate is rejected. Candidate and retained
ceiling counts are both3/6; no ceiling changes. The LARA rows overlap and are not added together.
Map total657628 is reference-only. All heap/stable page increases are0, not zero-allocation evidence.

For key64 the source removes one directory backing call without reducing bytes read, yet total
instructions rise. Missing key129 and tail keys select the right entry without needing its neighbor,
so they do not receive that call reduction. Buffer management, bounds/copying and codegen costs were
not isolated; read-count reduction alone is not an instruction-price model. No further cache variant
was tried. This result does not measure cover-first-write work; its subsequent experiment is
recorded in [section10](#10-first-cover-borrowed-window-retained).

### Correctness, complexity and restoration

The existing metadata-reader test was extended in its single owned map fixture: singleton
below-minimum insert/remove, two-entry canonical/prospective reads, then a staged split with a
changed middle entry and singleton odd tail. Exact read ranges, selected pages/positions and full
backing equality protect reuse, bounds and staged visibility. No constructor/install or test-count
increase. The baseline fails the new singleton-reuse assertion with two16B reads instead of one.
An initial new split expectation incorrectly assumed right length32/position0; inspecting the
existing32/33 split established length33/position1 for key64. Only those new expected values were
corrected, and both failed transcripts remain archived. This was not a production guard relaxation.

Candidate formatting, scoped clippy,28 unit tests and5 doctests pass. Both source-bound Wasm builds
and original filtered IC runs pass every unchanged fixture pre/postcondition. CSVs have exactly5/2
original rows/call counts; no results.yml was persisted. LARA's pending admission source is unchanged,
so this is not clean-commit integration. The retained baseline was reused, not rebuilt or rerun.

Lizard1.24.0: lookup CCN15→16/NLOC61→70/tokens444→528, parameters3 unchanged; its existing test
CCN6→9/NLOC50→133. Whole-file functions144→144 and CCN456→460. The added state is cohesive but has
no measured payoff. The entire lib, including all candidate-specific assertions, is restored
byte-for-byte; all28 retained tests and original benchmarks remain unchanged. No code is adopted.

Evidence: `$GLEAPH_EVIDENCE/adr0097/directory-pair-0356/`.
Candidate lib168458B/SHA256 `65a361628d903e951386e43e75a8cde2b08d36b783a8970d9f8fa42be9bb18c6`;
frozen map740308B/SHA256 `2c20c8f2e662fe37e4a1fdffcfe0032c78f145efcfe437319dc35ea148e447b7`;
frozen LARA5057163B/SHA256 `867297b11c0521aad700ebc5322a54b38962fe0120745c676bf6dbbefdc4273b`.
`map/wasm.json` and `lara/wasm.json` bind actual Cargo artifacts/features/source, including LARA's
default-only map rlib. `measurement.json`, native streams, `inspect/` and `restoration.json` preserve
the decision and exact restoration. No internal scope probes, second optimization or commit/push.

## 10. First-cover borrowed window: retained

**Status: implemented and measured; retained with a specific minor-control tradeoff.** Plan0357
adds11 lines inside `EditImage::cover` on source073473e4. A positive request on an empty range set
resizes the already-reserved byte storage, calls the existing gap filler once, appends one descriptor
at offset0, and returns the same borrowed window. No empty searches, byte relocation or coalescing
are needed. Count0 and every nonempty path retain their previous implementation exactly.

`Scratch` still owns capacity/representation; `cover` owns packed bytes, range initialization and
normalization. Callers still own no-op/full/live-prefix gap filling. No backing clamp, page rule,
second editor, cache, helper, public parameter, layout or dependency is added. The callback still
precedes descriptor insertion, and canonical publication remains after validation/admission.

This is not a retroactive adoption of historical F/G. Those used an older offset-returning cover,
were rejected for split regressions and did not measure LARA. This experiment uses the present
borrowed views, split/bulk movement and metadata-first insertion. It tests one placement, with no
internal probes and no second candidate; all original fixtures and28 unit tests remain byte-identical.

| Metric | Retained0353 | Retained0357 | Difference | Change | Original ceiling | Gate |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Missing64 | 89896 | 90152 | +256 | +0.2848% | 85672 | FAIL |
| Replace64 | 116872 | 105736 | -11136 | -9.5284% | 77576 | FAIL |
| Split/directory | 29581 | 29609 | +28 | +0.0947% | 30930 | PASS |
| Tail pairs64 | 406346 | 386954 | -19392 | -4.7723% | 298171 | FAIL |
| LARA total | 841909 | 832624 | -9285 | -1.1029% | 1527576 | PASS |
| LARA named | 840948 | 831663 | -9285 | -1.1041% | 1526615 | PASS |

Both target improvements pass. The missing increase of4 instructions per call and split increase
of28 are explicitly accepted for the measured replacement, tail and LARA improvements. These are
not summed across workloads; LARA's two rows overlap. No tolerance or ceiling changes. The three
achieved ceilings remain passed; missing/replacement/tail remain open. Map total617384 is reference
only. Every heap/stable page increase is0, not evidence of zero global allocation.

Missing executes no cover call, so its increase cannot be priced as execution of the new branch.
Source-bound static inspection leaves scalar/preparation frames at208/160B and scalar locals2;
preparation locals change40→41. Exact per-instruction causes are not isolated, and these are not
peak-stack measurements. Frozen map/LARA Wasm grow3130/6059B. This code-size increase and the two
minor runtime regressions are part of the accepted tradeoff, not omitted costs.

### Validation and evidence

The unchanged cover characterization passes before and after the change. It checks inline-first,
pre-reserved Heap48, empty/contained zero, disjoint insertion, ordered bridge gaps, suffix rebasing,
exact borrowed windows/packed bytes, capacities and unchanged backing. Two new intentional faults
are killed: removing the positive-count guard reaches empty-zero's callback assertion after three
positive cases (`[(376,0)]` versus `[]`); suppressing only the48B callback reaches the independently
verified heap-first callback assertion after two inline cases (`[]` versus `[(320,48)]`). Later
assertions in those failing cases are unreached. Both mutant sources/streams are preserved and the
candidate restored byte-exactly. No tests, Memory/map constructors or PocketIC installs were added.

Formatting, scoped clippy,28 unit tests and5 doctests pass. Both source-bound release builds and
original IC filters pass every unchanged fixture pre/postcondition. The CSVs contain exactly5/2
original14-column rows with expected calls; no results.yml was persisted. The0353 baseline is
reused, not rebuilt or rerun. LARA's pending admission work remains fixed, not clean-commit integration.
Lizard1.24.0: only cover changes, CCN12→14/NLOC79→89/tokens682→737, parameters4 unchanged;
whole-file CCN456→458/functions144 unchanged. No superseded helper or extra publication path remains.

Evidence: `$GLEAPH_EVIDENCE/adr0097/first-cover-0357/`.
Retained lib164795B/SHA256 `d750b05526e08723cf46cf2a3a10dfea8a95b9b56edd8e5bc007020e717182fa`;
frozen map743003B/SHA256 `dead4f3ddd9a3671bf3cabce6b1bd473a0a2b5d3f3f51b7bad210d26f62d1af0`;
frozen LARA5062516B/SHA256 `1a9716a10d12887728f5b4b95085d86d6537b867612e3bbdfd4c45d96367f446`.
Cargo bindings include LARA's default-only map rlib. `measurement.json`, `native/`, `inspect/` and
`completion/receipt.json` record the tradeoff, validation, codegen limits and exact protected scope.
This checkpoint includes no original-ceiling waiver, ADR0097 promotion or second optimization.

## 11. Single reservation owner: retained

**Status: implemented, measured and retained.** Plan0358 removes the independent production
fit/early-success decision from `StablePagedOrderedMap::reserve_scratch`, delegating normal calls
to the existing `Scratch::reserve`. The wrapper had its own checked live-length/capacity decision;
Scratch already implements fitting Inline/Heap requests and fallible growth. Those were alternative
implementations of the decision, not two checks necessarily executed on each missing call.

The only remaining wrapper predicate is cfg(test) observation: count and optionally inject a labelled
fault for a non-fitting request, then forward to Scratch unless that intentional fault returns.
Native fitting requests also execute the actual reservation owner. Scratch's capacity/allocation/
overflow/representation code is unchanged. The three consumers—previous results in prepare_image,
encoded bytes and descriptors in EditPlan::reserve—retain their order and error projections.
Lookup checks, prospective reads, first-cover behavior, full-page admission and the first canonical
write in publish_image are untouched. There is no new helper, type, parameter, mode, unsafe code,
public/storage API, dependency or editor. This does not revive0349's input specialization,0350's
unmaterialized state or0356's directory-pair read.

| Metric | Retained0357 | Retained0358 | Difference | Change | Original ceiling | Gate |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Missing64 | 90152 | 88872 | -1280 | -1.4198% | 85672 | FAIL |
| Replace64 | 105736 | 102792 | -2944 | -2.7843% | 77576 | FAIL |
| Split/directory | 29609 | 29531 | -78 | -0.2634% | 30930 | PASS |
| Tail pairs64 | 386954 | 381066 | -5888 | -1.5216% | 298171 | FAIL |
| LARA total | 832624 | 831214 | -1410 | -0.1693% | 1527576 | PASS |
| LARA named | 831663 | 830253 | -1410 | -0.1695% | 1526615 | PASS |

Every instruction control improves, supporting retention of the simpler delegation path. Missing
still exceeds its ceiling by3200; original acceptance remains3/6, not4/6. No gate, tolerance or
historical rejection is changed. LARA rows overlap and are not summed. Map total607194 is reference
only. Heap/stable page increases are0 in every row, not evidence of zero global allocation.

Map Wasm shrinks230B; LARA grows966B. The latter code-size cost is explicitly accepted for the
measured instruction benefit and removal of the production fit return. Scalar/preparation static
frames stay208/160B and locals2/41. Source-bound disassembly shows preparation now dispatching
directly on Scratch representation and its remaining-capacity check rather than the wrapper's
generic checked-sum/capacity selection. This is codegen corroboration, not isolated instruction
pricing, causal attribution of each workload's delta or a peak-stack bound. Missing's reduction
is20 instructions per scalar call in this fixture; that quotient does not price a generic reserve.

### Validation and evidence

The existing two-instance Scratch fixture gains fitting-reservation checks at exact remaining
capacity, overflow-failure byte/capacity/representation preservation, and nonempty Inline-to-Heap
prefix/length preservation. It passes before and after the delegation change. No original assertion
is removed. The existing ordered-edit test passes; a temporary fault that skips only multi-result
reservation survives scalar setup, prints the seven-intent marker, then fails at
`resize_reserved<Option<u64>,1>` while `prepare_image` pushes a prior. The backtrace identifies the
first batch preparation, not construction. Later prior/backing/discard assertions are unreached.
Mutant source and complete streams are preserved and the candidate restored byte-exactly.

Formatting, scoped clippy, all28 unit tests and5 doctests pass. Test count and Memory/map/PocketIC
constructor/install counts do not increase; the owner assertions reuse the two Scratch instances.
Both source-bound release builds and original filtered IC runs pass unchanged pre/postconditions.
CSVs retain exactly5/2 original14-column rows and expected calls. No benchmark source changes or
results.yml persistence. The0357 baseline is reused, not rebuilt or rerun; LARA's pending admission
changes remain fixed, so this is not clean-commit integration.

Lizard1.24.0 conventional CCN: reserve_scratch3→3, NLOC26→23, tokens162→153; the static parser
includes cfg(test) observation, whereas the production wrapper now only delegates. The expanded
owner test is CCN3→4/NLOC42→69/tokens372→579. Whole-file CCN458→459, functions144 unchanged;
no signature grows, no production abstraction is introduced and no superseded fit return remains.

Evidence: `$GLEAPH_EVIDENCE/adr0097/reservation-owner-0358/`.
Retained lib165868B/SHA256 `4e5456e152e8ddba352fd52d86f721bdf91aa234c01c42c429d2bc90d7f0c434`;
frozen map742773B/SHA256 `abad51cdadd3f2aaed4e59e9ac2c0010f11f5948c7070733350c8ba5a798384d`;
frozen LARA5063482B/SHA256 `cdcd861a113f7cee481406fec51e1e3e6517483f54246075820d7ecbe4100de7`.
Cargo records bind the linked default-only map rlib as well as both Wasm outputs. Source snapshots,
`mutation.json`, `measurement.json`, native streams, `inspect/` and `completion/receipt.json` bind
the retained decision and exact scope. No second optimization, benchmark sweep, commit or push is
part of this experiment.

## 12. Structural options beyond local map tuning

**Status at this research checkpoint: proposals only.** Option A was subsequently tested and
rejected in [section13](#13-order-preserving-rekey-rejected); B was implemented and retained in
[section14](#14-compact-page-header-retained). C remains unimplemented.
The most promising broader distinction is between reducing work inside one map edit and reducing
how many map edits an allocator operation requires. These address different gates. A FreeSpan-only
change cannot close the three remaining scalar ceilings, even if it substantially improves LARA.

### Source scope and observed execution

The inspected worktree is `$REPO` at commit
`b80566095f1edcec72158185cc27051bab9e2f0d`. Map source is the retained section11 `4e5456e1`.
`lara/edge/free_span.rs` includes pending work:78962B/SHA256
`35b7b00899db94ef58f6f6777ae2b29f9e6bc4a3c46a5dd4762e7067f87349fd`.
This is analysis of that development source, not clean-commit integration evidence.

Production consumers found under `crates/` use the paged map through `FreeSpanStore::by_start`
(`free_span.rs:249–253`). The free store is used for edge-slab, bucket-descriptor and inline-property
byte allocations; examples are `lara/edge/span.rs:16–62`, `labeled/bucket_store.rs:484–504` and
`lara/edge_inline_property.rs:751–803`. Units and ownership remain distinct between those stores.
The spill-run class allocator and native LTB block free list are separate mechanisms, not additional
users of this by-start map. Normal adjacency scans do not traverse this allocator; the
[Free-span contract](../storage/lara.md#4-free-span-contract-core-lara) and
[DGAP alignment](../storage/lara-dgap-contract.md#free-span-when-to-use) already keep in-window slack
out of FreeSpan and retire pinned edge storage at leaf granularity.

Source-derived work, not IC attribution:

- A basic map scalar call constructs an image and uses the common prepare/apply/admit/publish flow.
  Missing stages nothing; replacement stages8B; a tail pair stages26B then10B. The retained engine
  already has one editor and an allocation-free publisher. Splitting those into a second scalar
  implementation would duplicate its semantic rules.
- `take_best_fit`'s split arm (`free_span.rs:653–688`) and `replace_existing_span`
  (`1045–1065`) express a changed start as `remove(old_start)` plus `insert(new_start, id)`.
  Ordinary split followed by prefix restoration therefore invokes four scalar map edits. Bins,
  span records, summaries and the transient maximum heap are also maintained.
- The two replacement paths own the same span-change rule under different entry conditions:
  selection already gives `take_best_fit` an id; `replace_existing_span` first verifies an exact
  old span. They must converge on one FreeSpan mutation owner before changing that rule. Preserve
  their different admission checks. Bin-prepend mechanics also appear in both
  `write_active_record` and `relink_active_record`.
- `pick_span_in_bin` (`931–981`) may traverse the entire remaining start-bin list after its first
  eight probes. `record_max_candidate`, `rebuild_max_heap` and `rebuild_largest_free_span`
  (`1256–1348`) maintain a lazy heap and may rebuild it. These are separate scaling and cold-state
  costs; their share of the fixed1024-span gate is not established.
- `allocate_span_avoiding` (`lara/edge/span.rs:20–62`) currently takes a span, releases the taken
  prefix if it overlaps the avoided interval, then falls through to tail allocation. A prospective
  allocation decision could avoid this speculative canonical mutation. It cannot be substituted
  for the original benchmark's externally completed take and restore operations.

### A. Represent an order-preserving span change directly

This refines section5's rekey alternative. For disjoint free spans, taking a proper prefix changes
`[p,q)` to `[p+m,q)`, retaining the same SpanId. No other free start lies inside the original span,
so the indexed key changes without changing its rank. Restoration has the same opportunity only
when overlap/duplicate checks establish that the expanded span is valid.

An explicit, general-purpose order-preserving rekey operation in the map could update the entry's
key and, when that entry is first in its page, its directory minimum. It would not remove an entry,
shift a suffix, change occupancy or split a page. Map ownership includes proving predecessor/new-key/
successor ordering, including cross-page neighbors; FreeSpan ownership includes non-overlap, exact
prefix allocation, SpanId retention, bin membership and summaries. Neither may trust the other's
unstated proof. In the ordinary split/restore case, four map edits could become two rekeys. Those
are operation counts, not a speedup estimate, and existing Insert/Remove intents must not silently
be rewritten or cancelled.

Compare separately:

- Existing ordered map batching amortizes preparation but retains remove/insert semantics and
  their physical shifts. It is the minimum-integration alternative.
- Explicit rekey removes the semantic need for those shifts. It needs a new map operation/error
  contract and audit of every enum consumer; absent keys, collisions, order violations and
  DirectoryFull behavior must be specified rather than inherited accidentally from remove/insert.
- The planned FreeSpan/Span preparation in ADR0088 can aggregate record/bin/header/index changes
  and keep allocation private until publication. It is a broader atomicity/integration project,
  not already implemented by placing two map intents in a batch. Current FreeSpan production
  source has no `prepare_edits` call; the similarly named local `SpanPlan` in `graph/values.rs`
  is unrelated. Selection with an avoided interval belongs to that existing allocator owner.

No persistent map format change is intrinsically required for rekey. Prepared allocator wiring
must still cover heap state, physical versus logical capacity, aliases and all entry points as
specified in ADR0088. These are alternatives or successive decisions, not one bundled experiment.
**Best fit:** reduce real allocator edit/search/write work across LARA consumers. **Direct effect on
missing/replace/tail scalar gates:** none.

### B. Make the page format match selected-page reads

The map's32B page header stores `flags`, `len`, `prev`, `next` and `free_next`
(`lib.rs:77–96,1289–1338`). Active pages require `free_next == 0`; freeing a page constructs
`len=0, prev=0, next=0, free_next=head` (`remove_page:999–1006`). Active-chain links and the free-list
link serve mutually exclusive states. Yet every selected active page currently requires a32B header
read followed by an8B first-key read for directory-minimum agreement (`page:711–737`,
`lookup:1040–1060`).

A concrete layout alternative is a24B tagged header:

| Page state | First8B | Second8B | Third8B | Entries start |
| --- | --- | --- | --- | --- |
| Active | Tag, length, defined padding | Previous page | Next page | offset24 |
| Free | Tag, zero length, defined padding | Next free page | Defined unused bytes | offset24 |

This reuses a link slot according to the tag; it does not narrow u64 page ids or duplicate the first
key in metadata. A selected-page prefix of32B then contains the complete24B header and the first8B
key. Proposed geometry is24+64*16=1048B per page, replacing1056B. The shared decoder must define
unknown tags and inactive-field bytes; the Rust representation should not expose simultaneous
active and free links as independent valid state.

For the existing valid scalar fixture, a combined prefix read could remove one backing read and
8 transferred bytes per selected-page lookup:64 calls for missing,64 for replacement and128 for
tail pairs. This is a planned traffic reduction, not an instruction price or measured improvement.
It retains the32B maximum read size rather than relaxing that oracle to allow the previously
considered40B read. It also needs no neighbor cache or adjacent-directory prefetch.

The full allocated-page extent must be checked before reading the prefix. Active tag/length/link
checks and directory/first-key agreement remain, in their semantic error order. Old-format
`active.free_next != 0` is replaced by an explicit variant encoding; there is no longer a separate
active free-pointer field to validate. That is an intentional schema change, not deletion of the
current lookup's check. Free-list bounds/cycles and recycle transitions need equivalent coverage
under the new representation. Read-only map operations, prospective newly allocated pages, split,
recycle, overflow arithmetic and all codec users must share the one new layout.

**Cost:** an ADR decision and fresh state for affected backing; no migration or legacy decoder.
The pre-production change keeps the existing version marker rather than introducing a new epoch. Tests with literal offsets and arithmetic limits need independently
derived new expectations. Basic dirty-write contracts can remain8/26/10B. Section11 baseline
artifacts remain reusable, but byte snapshots are valid only within each layout; logical fixture
contents, operations and instruction ceilings remain the comparison contract. This proposal does
not include a map-wide header-field reorder, directory redesign or second optimization.
**Best fit:** a structural common-read candidate directly relevant to all three unmet scalar gates.
The remaining394/call replacement and about648/call tail gaps are not promised by one fewer read.

### C. Replace the allocator's index combination

A more radical alternative would put address ordering into the existing span records themselves,
using a balanced intrusive tree with a `subtree_max_len` aggregate. One record remains authoritative
for start/length; child/parent links and subtree aggregates are maintained by the same FreeSpan
mutation owner. Address neighbors, a fitting-span search and largest-span lookup could then use
that structure instead of the separate paged by-start map, bin lists and stale maximum heap.

For a valid balanced tree, searching the leftmost subtree whose maximum length reaches a request
can find the lowest-address fitting span inO(log F); largest length is available at the root.
Prefix split/rejoin preserves address rank, so it need not remove/reinsert the node, though length
aggregate changes may propagate to ancestors. This is an algorithmic derivation, not an IC result.
Arbitrary insert/remove still requires balancing and prepared writes to every affected ancestor.

The important tradeoff is **selection policy**: address-first-fit is not the current bounded
best-fit/first-fit fallback policy. Completeness (never miss a usable span) and exact-prefix
allocation can be retained, but fragmentation and placement change. If size-oriented selection is
required, compare an address index plus a `(length,start)` index instead; that retains two update
paths and their consistency work. XFS is a primary-source example of separate address and size
indexes, not evidence that two generic StableBTreeMaps are fast on IC. The current FreeSpan module
already records replacing such a secondary BTree with bins; simply undoing that choice has no
present performance support.

This alternative requires new persistent record/root fields, fresh state, replacement of old index
and heap paths, and a new reopen-integrity proof. It may reduce structures while increasing stable
random reads and ancestor writes. Direct scalar map gates do not improve if LARA stops using the
map: removing its consumer does not waive those gates or justify deleting the crate's contract.
**Best fit:** a separate allocator design decision when fragmentation, bin tails or heap lifecycle
costs justify it. **Current recommendation:** compare the policy/cost model, not implement yet.

### Other broad alternatives and exclusions

| Alternative | Work it could remove | Why it is not the first structural choice |
| --- | --- | --- |
| Separate prospective edit state from final packed dirty-byte emission within the same editor | Repeated packed-byte relocation during staging | Does not directly help zero-write missing; bounded scratch, overlap composition, ordered errors and priors must be proved. An unbounded append log or whole-page copy is not an acceptable replacement |
| Multi-level directory / slotted-page map | Large-directory suffix movement / interior payload shifts | The fixed gate hasD=2; tail shifts no entries. Added indirection and persisted metadata need large-D/interior evidence, not a claim about current missing/replacement |
| TLSF or size-class bitmap alone | Empty-bin scanning | Does not remove partial-bin fit search, start-order coalescing or index mutations. Section4's incomplete-search warning remains binding |
| Remove FreeSpan, or substitute fixed-size block pools everywhere | Allocator index maintenance | Variable-length exact-prefix reuse and adjacent leaf expansion are real consumers. Spill-run classes and LTB block pools have different allocation contracts |
| General cache, COW/MVCC, MemoryManager replacement | Various repeated reads or copies | No source-bound attribution justifies their state/consistency cost. The scalar fixture uses direct DefaultMemoryImpl, while LARA's fixture uses VirtualMemory; their call costs cannot be interchanged |

### Boundary assessment and next decision

- **Encapsulation:** B remains wholly inside the generic map. A exposes a generic ordering operation,
  never FreeSpan record or bin internals. C replaces FreeSpan's internals, not Graph/Router contracts.
- **Separation of concerns:** map owns ordering/geometry; FreeSpan owns free-interval selection and
  consistency; EdgeStore owns slab capacity and placement. General GQL crates are unaffected.
- **Invariants:** nonpublication, full-page admission and ordered failures remain for A/B. C requires
  equivalent allocator atomicity plus an explicit selection-policy decision, not weaker corruption
  handling. ICP trap rollback is not a replacement for preparation.
- **Consistency:** A first reconciles the two span-replacement implementations. B has one tagged
  codec/layout. C must delete superseded indexes/heap when replacing them, while treating subtree
  maxima as derived state with a single maintenance owner.
- **Fitness:** A removes operations the allocator's interval semantics do not need. B removes a
  mutually exclusive stored field from the common read unit. C has greater conceptual scope but
  cannot be preferred without accounting for ancestor writes and fragmentation.

For **the three scalar ceilings**, B is the strongest concrete structural question found here.
For **LARA-wide improvement**, A is the first design comparison, with ordered batching as its
simpler control and C as the larger replacement alternative. They must not be bundled into a
single implementation slice. The earlier fixed-slot result-storage proposal remains a small,
unmeasured bookkeeping idea, not the recommended answer to this broader question.

Before adoption, B needs the complete layout/invariant inventory; A needs a precise operation/error
contract and one FreeSpan replacement owner; C needs a selection-policy decision and a bounded
read/write/fragmentation comparison. Relevant existing tests and focused measurements should follow
the chosen scope. No additional Rust/IC execution occurred in this research. Original acceptance
remains3/6, and none of the proposals establishes an instruction gain.

External primary sources inspected for C: Linux
[augmented rbtree documentation](https://docs.kernel.org/core-api/rbtree.html#support-for-augmented-rbtrees)
requires propagation/copy/rotation maintenance for derived subtree data; its interval example uses
maximum endpoint, whereas maximum *length* above is this proposal's distinct aggregate. XFS
[Algorithms & Data Structures](https://www.kernel.org/pub/linux/utils/fs/xfs/docs/xfs_filesystem_structure.pdf)
§§13.2/13.2.2, printed pp.55/57, describes free-space trees sorted by block address and extent size.
Neither source supplies Gleaph/IC performance evidence.

## 13. Order-preserving rekey rejected

Plan0359 tested section12 option A alone against the retained0358 source. The candidate added
`Edit::Rekey { key, new_key }`, scalar `rekey` and `Error::OrderViolation` to the existing generic
map editor. Source lookup and a second prospective insertion-position lookup proved that the key
would keep its rank. It wrote only the key and, for a page minimum, its directory minimum. No
layout, allocation policy, benchmark input or measured closure changed.

FreeSpan's split path and existing replacement path converged on `replace_existing_span`;
`write_active_record` delegated its duplicated bin prepend to `relink_active_record`. The shared
replacement retained exact-span admission, rekeyed before bin mutation, then maintained record,
heap and summaries once. This deliberately accepted the split caller's extra exact lookup rather
than adding a separate known-id fast path. Composite FreeSpan preparation remained planned A0.

Candidate map:177203B/SHA256 `ff10dce9c99e0331e765a6370a406fe2e2f403c33bc390ca5746d8b1336663eb`.
Candidate FreeSpan:79526B/SHA256 `45acd5a221c1583ebaf155d951b706f2b7750e5a8fa8dd56ab4a3b387f55f432`.
LARA includes the same pre-existing pending admission changes, not clean-commit integration.

| Metric | Retained0358 | Candidate0359 | Difference | Change | Original ceiling | Gate |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Missing64 | 88872 | 89704 | +832 | +0.9362% | 85672 | FAIL |
| Replace64 | 102792 | 103624 | +832 | +0.8094% | 77576 | FAIL |
| Split/directory | 29531 | 29544 | +13 | +0.0440% | 30930 | PASS |
| Tail pairs64 | 381066 | 383562 | +2496 | +0.6550% | 298171 | FAIL |
| LARA total | 831214 | 632242 | -198972 | -23.9375% | 1527576 | PASS |
| LARA named | 830253 | 631281 | -198972 | -23.9652% | 1526615 | PASS |

**Decision: reject the whole candidate.** Both LARA rows improve substantially, but all four map
controls regress. Original acceptance remains3/6. No tradeoff allowance, aggregate substitution,
partial retention or second optimization was applied. The LARA rows overlap and are not additive.
These are joint candidate costs, not isolated prices for rekey, dispatch or search. All reported
heap/stable page increases are zero, which does not establish zero allocation.

Two new map tests cover exact8/16-byte writes, unchanged unrelated bytes/occupancy/value/identity,
inline storage, extreme keys, missing/equal keys, collisions and rank crossings, ordered prospective
neighbors, discard, late exact errors and previous-page corruption. Existing full-directory and
unbacked-page resource fixtures also exercise rekey without weakening their prior assertions.
The existing FreeSpan split/restore/reopen fixture additionally checks SpanId, complete contents,
index header and summaries for best-fit and exact-prefix entry points. Map30 units/5 doctests and
FreeSpan27 units pass; correctness tests add two native map fixtures, no PocketIC installs.
A rank-guard bypass mutant completes all valid/no-op cases, then fails the exact `64 -> 66`
collision assertion (`Ok(Some(...))` instead of `OrderViolation`). It was restored byte-for-byte.
The initial test-only integer-inference compile error was fixed with an explicit u64 argument type.

Formatting, strict map lib/tests clippy and strict LARA lib clippy pass. The combined strict
lib/tests clippy command fails on43 pre-existing diagnostics in14 unchanged LARA test files; it
was neither suppressed nor treated as passing. See
[GAP-LARA-TEST-CLIPPY](../implementation-gaps.md#gap-lara-test-clippy--strict-test-target-lint-is-blocked).
Both original source-bound IC filters complete with exactly5/2 original CSV rows; the baseline is
reused without rebuilding or remeasurement. Timed Rust/IC commands total130s.

Lizard1.24.0, conventional `--csv -i -1`, explicit two-source before/candidate snapshots:
map lookup CCN15/NLOC61 unchanged; apply_edit CCN32 unchanged/NLOC153→160; new rekey helper
CCN19/NLOC48 and scalar facade CCN1/NLOC3. FreeSpan take_best_fit CCN9→8/NLOC42→30;
write_active_record CCN6→3/NLOC29→9; relink CCN4 unchanged/NLOC22→23; replacement CCN5 unchanged/
NLOC18→14. The new helper has source location, old/new keys and a test-only ordinal, not a new
persistent structure. Branches enforce concrete rank and page-boundary cases; no blanket attribute
or separate scalar editor was introduced. Complexity is not the rejection metric; runtime controls are.
Map Wasm grows742773→744429B; LARA shrinks5063482→5062924B. Frozen binaries and linked map rlib
are source-bound in the evidence archive, not installed as retained artifacts.

The complete candidate Rust files, including new tests and API comments, were restored to the
pre-trial bytes: map `4e5456e1`, FreeSpan `35b7b008`. All2357 pinned inputs were equal before adding
this outcome and the lint-gap record. No API or deduplication fragment remains. Candidate sources,
patches, command streams, source/artifact bindings, CSVs, lint diagnostics and restoration proof live
under `$GLEAPH_EVIDENCE/adr0097/order-preserving-rekey-0359/`.

## 14. Compact page header retained

**Status: implemented, measured and retained.** Plan0360 tested section12 option B alone from
restored0358. The map's mutually exclusive active/free links share one wire slot, removing the
independent free pointer. The resulting24B header lets `EditPlan::page` return selected metadata
and its first key from one32B prefix, without widening the maximum-read contract. No rekey fragment,
neighbor cache, directory prefetch, scratch change or allocator-policy change is included.

The format and validation owners are specified in
[ADR0088, compact paged-map layout](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#paged-map-compact-layout).
The raw decoder retains unvalidated tag/length/link fields; it is not a second validated-page type.
`PageHeader::free_next()` is the shared free-shape validator for allocation and the full walk.
Previously those sites had separate checks. Full-page admission and directory/first-key agreement
remain in their original order. All page constructors, active/free transitions and readers use one
geometry. Only the current layout is supported; use fresh owning bundles when layout changes.
The existing version marker1 and public `HeaderV1` type remain unchanged. No old-layout detection,
migration, compatibility alias or isolated index rebuild is provided. FreeSpan records, bins,
selection, heap and all production consumers remain unchanged.

| Metric | Retained0358 | Retained0360 | Difference | Change | Original ceiling | Gate |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Missing64 | 88872 | 85352 | -3520 | -3.9608% | 85672 | PASS |
| Replace64 | 102792 | 99272 | -3520 | -3.4244% | 77576 | FAIL |
| Split-directory | 29531 | 29274 | -257 | -0.8703% | 30930 | PASS |
| Tail-pairs64 | 381066 | 374026 | -7040 | -1.8474% | 298171 | FAIL |
| LARA total | 831214 | 823204 | -8010 | -0.9637% | 1527576 | PASS |
| LARA named | 830253 | 822243 | -8010 | -0.9648% | 1526615 | PASS |

Every control improves, so the whole candidate is retained. Missing clears its original ceiling
by320 instructions; original acceptance advances from3/6 to **4/6**. Replacement remains21696 over
its ceiling and tail75855 over. This is not full ADR0097 acceptance or permission to waive the map
controls if a later allocator design no longer uses the map. Map aggregate607194→592857 is context,
not a seventh gate; LARA total/named scopes overlap and must not be added. All measured heap/stable
page increases remain0, not a peak-memory or zero-global-allocation claim. These are whole compiled
candidate deltas, not per-read/check prices.

Validation preserves the intended contracts rather than reusing the former page offsets:

- `page_header_codec` fixes both literal24B encodings; `page_reopen_and_recycle` uses one native
  backing/map to prove the unchanged version marker, two-link free-list formation, malformed
  free tag/length/unused word, self/out-of-range links, full-walk cycle rejection, exact reuse of
  page2 without increasing page count, logical contents and reopen. It adds no PocketIC fixture.
- Literal overflow witnesses were independently derived for the1048B stride: the last fitting page
  start is17601855032165478 at18446744073709551032; its full extent overflows. The next start also
  overflows. The read-partition fixture retains its byte/traffic assertions but admits wholly
  unbacked page64 at197160, rather than relying on the old page63/Wasm-boundary coincidence.
- Existing canonical/prospective reader tests require the32B prefix and forbid a separate first-key
  request. Scalar read assertions are tightened to exact calls/bytes: replace7/96, missing9/104,
  tail insert10/112 and tail remove11/120. Exact8/26/10B dirty writes, inline/no-grow behavior and
  max-read32 are preserved. Split-position, staged-source, real backing/growth and ordered-failure
  tests remain in the full map suite.
- All30 map units,5 doctests and27 unchanged FreeSpan tests pass on the corrected source. The initial
  trial's deliberate minimum-comparison bypass made `prepared_edits_reject_inconsistent_minimum`
  fail with “minimum mismatch unexpectedly reached ready” (rc101), after successful setup. That
  guard and its test are unchanged; the mutation was not repeated for the version-policy correction.
  Formatting and map lib/tests plus LARA lib strict clippy pass.
  The unchanged [LARA test-target lint gap](../implementation-gaps.md#gap-lara-test-clippy--strict-test-target-lint-is-blocked)
  remains open; the already-failing combined command was not rerun or relabeled as passing.
- Both unchanged IC filters complete with exactly5 map/2 LARA CSV rows, named calls1, new status and
  empty deltas; no baseline rebuild/remeasurement or persisted results. Timed Rust/IC commands use
  115s for the correction, each below300s. Both CSVs are byte-identical to the initial114s trial's
  outputs. No full-workspace or general PocketIC suite was run.

Lizard1.24.0, conventional `--csv -i -1`, original0358 versus corrected frozen candidate:22 directly
touched functions, max CCN21→21, aggregate CCN148→154/NLOC1473→1558 including tests. Production
changed-function totals are CCN100→101/NLOC432→420. `page` CCN10→9/NLOC27→28, `lookup`15→14/61→58,
allocation11→10/30→27, codec decode25→20 NLOC and encode13→11; shared `free_next` adds CCN4/NLOC6.
The shared validation replaces separate allocation/walk checks; growth is the two focused tests,
not an execution framework. No input parameter is added, no5+-parameter signature grows, and the
sole changed return shape adds the key read with its header. The existing public header type is
preserved. Review verdict: focused; retain the single layout/read change, with no further
extraction or generalization. CCN is not performance evidence.

Evidence: `$GLEAPH_EVIDENCE/adr0097/compact-page-header-0360/preproduction-correction/`.
The initial trial archive in the parent directory is unchanged. The correction restores the existing
version marker/type and removes only the superseded version-rejection scenario; codec/editor
production logic is otherwise identical. The baseline is the immutable0358 archive.
Retained lib169562B/SHA256 `6bb465d5d7cfbe089dd8ff43759a557be051f4063e932222d231e022ea8853e1`;
map742537B/SHA256 `e255c9c26b5cd716aa8bdf2c33ee4607249aa092984ddccef41dc7f5232bd1cd`;
LARA5063120B/SHA256 `e18d789ec3deaf21dda2d5357399b9d9119c820f09631a41fc8d5a31989cda28`.
Map/LARA binaries shrink236/362B. The map Wasm was frozen before building LARA; LARA's default-only
map rlib is archived separately. Command streams, lock/profile/features, source bindings, CSVs,
complexity comparison and the correction receipt are retained there. Geometry and the original
mutation evidence are reused from the parent archive.
LARA still includes the unchanged pending admission work, not clean-commit integration.

## 15. Reserved append retained

**Status: implemented, measured and retained after the disk-capacity interruption.** Plan0361 tested
one candidate on the shared write path after the compact-header checkpoint. Earlier0354/0355 scope probes are
observer-affected measurements of an older source, not current component prices. The rejected
0349 generic-input and0350 lazy-Empty candidates were not retried. First-cover and single-reservation
changes already belong to the retained implementation.

The selected candidate changes only `Scratch::push_reserved`, shared by first dirty-range creation
and per-intent prior storage. It appends a supplied initialized value directly instead of taking a
checked current slice, calling generic resize, then overwriting the new slot. The existing Inline
array checks its destination before length changes; Heap verifies spare reserved capacity before
Vec::push. Bulk resize, fallible reservation, initial representation, the edit engine, codecs,
layout and publication are unchanged. No new production helper, type, mode or parameter is added.

The frozen0360 Wasm contains concrete removable work in the DirtyRange append at0x8a9c: two inline
length comparisons at0x8ab9/0x8ac0, and a heap zero-vector store at0x8b2f before the supplied value
store at0x8b6c. These establish code presence, not executed-path frequency or isolated IC prices.
In particular, scalar inline appends do not execute that heap zero store. No current bottleneck
ranking or predicted saving is inferred from these observations.

One existing pure Scratch fixture was extended, with two additional cheap Scratch instances and
no new Memory/map/PocketIC constructor or install. A local Default counter rejects a discarded
initial value; exact values, full-capacity rejection and unchanged length/content/capacity/variant
protect both representations. Old production passes the inline cases, including its caught
full-capacity panic, then fails exactly on the first heap append: Default count1 instead of0.
The candidate passes the complete fixture. A deliberate inline length-before-index mutant reaches
the full-capacity failure and is caught by the unchanged-length assertion,2 instead of1. It is
restored exactly; no mutation code is retained.

### Interrupted validation and exact resumption

The initial candidate passed formatting, strict map lib/tests and LARA lib lint,30 map units and5
doctests. FreeSpan compilation then failed writing its incremental `query-cache.bin` with
`No space left on device (os error 28)`, before any FreeSpan test ran. At132MiB available, execution
stopped; the complete lib and tests were restored to0360. The error, incomplete plan and restoration
of all2357 input pins remain archived. No speculative retry, build-mode workaround or cache deletion
was used to conceal that failure. The initial timed commands used35s.

After the user freed storage,73GiB was available. The resume verified all input pins, the exact
blocked research state, compiler/lock versions, canbench0.7.0/PocketIC13 and frozen baseline
artifacts, then reinstated the identical candidate. Successful native checks and both negative
witnesses were reused, not rerun or attributed to another source. The filtered FreeSpan command
now passes27 tests (652 filtered); both release Wasm builds and the two unchanged original IC
filters pass. Resume commands use75s, each below300s. The unchanged strict LARA test-target lint
debt remains open; it was not rerun, suppressed or relabeled as passing.

### Source-bound measurements and decision

| Metric | Retained0360 | Retained0361 | Difference | Change | Original ceiling | Gate |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Missing64 | 85352 | 84008 | -1344 | -1.5747% | 85672 | PASS |
| Replace64 | 99272 | 96584 | -2688 | -2.7077% | 77576 | FAIL |
| Split/directory | 29274 | 29185 | -89 | -0.3040% | 30930 | PASS |
| Tail pairs64 | 374026 | 368266 | -5760 | -1.5400% | 298171 | FAIL |
| LARA total | 823204 | 821014 | -2190 | -0.2660% | 1527576 | PASS |
| LARA named | 822243 | 820053 | -2190 | -0.2663% | 1526615 | PASS |

**Decision: retain the whole candidate.** Replacement and tail strictly improve and none of the
other four controls regresses; in fact all six improve. Original acceptance remains **4/6**:
replacement is19008 above its ceiling and tail70095 above. Missing has1664 instructions of margin.
No ceiling, scalar contract or acceptance metric is relaxed. The overlapping LARA rows are not
additive. Map aggregate592857→582976 is context, not a seventh gate. Every reported heap/stable
page increase is0, not evidence of zero allocation or a peak-resource bound. Five/two original
14-column CSV rows, named calls1, new status and empty deltas are verified; nothing is persisted.
The0360 baseline was neither rebuilt nor remeasured.

Lizard1.24.0, conventional `--csv -i -1`:3 directly touched functions, CCN5→13/NLOC73→128,
touched maximum4→10. Production append is CCN1→2/NLOC4→12; the rest is the existing test expansion
and its local Default implementation. Whole-file CCN465→473/NLOC3810→3865. The final live-source
measurement matches the archived candidate in every per-function field. Both representation cases
belong to the existing Scratch owner; no five-plus-parameter signature grows, and no further
abstraction or scalar-specific path is justified. CCN does not establish the measured speedup.

Map/LARA Wasm grow677/1284B, explicitly accepted for the measured improvement. Frozen codegen
keeps scalar/preparation static frames208/160B; their local counts are2/41→2/40. The candidate
has no named standalone DirtyRange append function in the disassembly. That is a compiled-shape
observation, not proof that append behavior disappeared or an isolated price for a check/store.
Static frames do not establish peak stack use.

Retained lib171834B/SHA256 `7c2c11fd1aab6909a84df951600726b3713e4d0510bae0df8b244698a1daea40`;
map743214B/SHA256 `5faa094a178948d0241bdcac7ebf4669dfcd71f856522636ea2759253f789398`;
LARA5064404B/SHA256 `d3b332637bcda2e0b2824cf4758c4010b88d2f9a2d8fab683c8435e6ccac0312`.
The map Wasm was frozen before LARA rebuilt its default-only map dependency; that linked rlib is
archived separately. LARA still includes the unchanged pending admission work, not a clean-commit
integration validation. No full workspace/PocketIC run, second candidate, commit or push was made.

Evidence: `$GLEAPH_EVIDENCE/adr0097/scratch-resize-dispatch-0361/`.
The original sources, failed compiler stream, negative witnesses and blocked completion receipt
remain immutable. `resume/` contains the input verification, successful commands, Cargo artifact
bindings, frozen Wasm/rlib, CSVs, measurement decision, final codegen/Lizard review and completion
receipt. The initial restoration is historical; the exact candidate is now retained in the live lib.

## 16. Reserved resize retained

**Status: implemented and retained.** Plan0362 makes one representation-local reserved-resize
change from committed map source `7c2c11fd` / commit `79702eefa`. Replacement and tail improve;
missing is equal, and split plus both LARA controls improve. Original acceptance remains **4/6**,
not full acceptance. The remaining gaps are18496 instructions for replacement and67919 for tail.

### Shared owner and concrete residual work

In0361, `Scratch::resize_reserved` selected capacity through `Scratch::capacity`, then matched the
same Inline/Heap representation to resize and return a view. Inline's checked array prefix repeated
its capacity bound. All four production calls remain in `EditImage::cover`: first byte window,
general byte growth, descriptor growth, and descriptor equal/shrink. The0361 direct-append path
is unchanged.

The frozen0361 map Wasm5faa094a contains this sequence inside `EditImage::write` at0x1cb20:

| Operation | Wasm address |
| --- | --- |
| Select representation-dependent capacity and compare requested length |0x1ccba/0x1ccbb|
| Dispatch on the representation |0x1ccc5|
| Assign Inline length |0x1cd69|
| Check the Inline array-prefix bound again |0x1cd70|

Its insert-cover monomorph at0x7dba has the same sequence at0x7f4c/0x7f4d,0x7f57,0x7ff4,0x7ffb.
The first check made the later length-before-slice ordering safe: redundant work, not a demonstrated
corruption defect. Source tracing gives one bulk resize per replacement, five per tail insertion
and three per tail removal; missing has none. These are source-level executions and static compiled
instructions, not isolated prices. No instrumentation or baseline rebuild/remeasurement was added.

### One retained change, unchanged contracts

`Scratch::resize_reserved` now matches once. Inline obtains the checked mutable array prefix before
updating length and returns that same view. Heap checks reserved capacity before the unchanged
`Vec::resize(new_len, T::default())`. The common capacity selection is removed; necessary bounds
remain. `Scratch::capacity` is now cfg(test)-only, retaining existing observations without dead
production code or a lint suppression. The resize inline attribute is unchanged.

Exact returned length/write-through, capacity and representation are preserved. Over-capacity
rejection precedes length/content changes, Default initialization and implicit growth. Inline
retains initialized hidden slots when shrinking; Heap initializes newly exposed slots when
regrowing. No append/reserve/caller algorithm, layout/version/HeaderV1, public API, resource-error
order, full-page admission or publication boundary changes. No helper, mode, cache, dependency or
second scalar editor is introduced.

The existing fixture still constructs only four cheap Scratch instances, with no Memory/map/PocketIC
setup added. It now checks zero/exact capacity, equal length, write-through, distinct suffix regrowth,
and both capacity+1 and usize::MAX rejection. It restores the nonzero prefix before the existing
reservation assertions. Raw enum length is inspected before as_slice after rejection; exact values,
capacity, variant and Default count remain fixed. Every old assertion is retained.

The completed fixture passes on unchanged production and on the candidate. The Inline mutant
assigning length before bounds fails at the unchanged-length assertion (2 instead of1). The Heap
mutant without its capacity guard returns from capacity+1 resize and fails the expected-panic
assertion; it never reaches Heap usize::MAX. Each final mutant was run separately and the exact
candidate restored between them and before final validation. An initial fixture pass lacked the
planned explicit equal-length call; that omission was corrected and the focused four-command cycle
repeated before final native/IC validation. Initial and final evidence are both retained.

### Source-bound measurements and decision

| Metric | Retained0361 | Retained0362 | Difference | Change | Original ceiling | Gate |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Missing64 |84008|84008|0|0.0000%|85672|PASS|
| Replace64 |96584|96072|-512|-0.5301%|77576|FAIL|
| Split/directory |29185|29123|-62|-0.2124%|30930|PASS|
| Tail pairs64 |368266|366090|-2176|-0.5909%|298171|FAIL|
| LARA total |821014|819559|-1455|-0.1772%|1527576|PASS|
| LARA named |820053|818598|-1455|-0.1774%|1526615|PASS|

**Decision: retain the whole candidate.** Both strict improvements and all four non-increase controls
pass. No original ceiling is relaxed. Map aggregate582976→580226 is context, not a seventh gate;
the overlapping LARA rows are not additive. All measured heap/stable page increases are0, not a
zero-allocation or peak-resource claim. Five/two original14-column CSV rows, exact names, named calls1,
new status and empty deltas are verified. Fixture bytes are unchanged; no results.yml was persisted.

Final codegen confirms the intended removal in both inspected resize sequences. `EditImage::write`
now dispatches at0x1ccbc, guards Inline's bound at0x1cce4, then stores length at0x1cceb; Heap's capacity
guard remains at0x1ccc7. Insert-cover dispatches at0x7f44, checks Inline at0x7f6c, stores at0x7f73 and
retains Heap's guard at0x7f4f. Neither selected sequence contains the earlier capacity-select step.
These addresses establish compiled ordering, not individual instruction prices or peak stack use.
Map/LARA Wasm grow16/28B; that bounded size cost is accepted for the measured improvement.

Lizard1.24.0 with conventional `--csv -i -1` measures three touched functions, including the capacity
accessor's cfg-only change: CCN14→19/NLOC131→189, maximum10→15. Resize stays CCN2 and grows13→14 NLOC;
the existing fixture grows112→169 NLOC and10→15 CCN. Whole-file CCN473→478/NLOC3865→3923, with148
functions and maximum32 unchanged. No signature, type or abstraction is added. The explicit variant
cases protect different existing initialization/rejection contracts; splitting them into generic
test machinery would obscure those witnesses. Complexity metrics do not establish speedup.

Formatting, strict map lib/tests and LARA lib lint,30 map units,5 doctests (1+4 harnesses),27 FreeSpan
tests (652 filtered), both release builds and both original filtered IC runs pass. Timed commands
consume100s, each below300s. A shell-only empty-argv/nounset error before the native test batch was
fixed by forming one nonempty argv; no Rust command had started in that failed invocation. Existing
GAP-LARA-TEST-CLIPPY remains open, not rerun or waived. LARA includes unchanged pending admission work,
not clean-commit integration evidence. No full workspace/PocketIC run, second candidate, commit or push.

Retained lib174449B/SHA256 `47796acb7e26ecdd138570922702d12968bd5c52983e89dfc745f436ff1b79e5`;
map743230B/SHA256 `5fcff73dfc47b3bd3252bb105d5c3f0453031a407bc43951c3115f8e9c1e8e08`;
LARA5064432B/SHA256 `daa62418a1ac8566dcac92cf0f7f5a389dea3aef084d892f99f202ed284b5a45`.
The map Wasm was frozen before LARA rebuilt its default-only map dependency; the linked rlib is
archived separately. The next decision is not selected by this relative improvement.

### Alternatives remain separate

Publication's byte-storage dispatch was already outside its emitted write loops (0361:0x1ca2e).
Selected-page checks enforce distinct invariants. Moving prior reads into lookup would change its
result contract. Neither is bundled into this owner-local change. Earlier input/Empty-scratch/
directory-pair/rekey rejections stay closed; observer-affected0354/0355 scope counts do not rank
current costs. LARA index replacement remains a separate policy decision.

Evidence: `$GLEAPH_EVIDENCE/adr0097/scratch-resize-owner-0362/` contains input
pins, all characterization/mutant sources and transcripts, final candidate checks, Cargo/source/lock
bindings, frozen Wasm/rlib, original-filter CSVs, codegen/Lizard and the completion decision/receipt.

## 17. Paired edit-start metadata read rejected

**Status: measured/rejected; full restoration complete (Plan0363).** The paired read worked, but
split regressed and the improvements did not close the two open ceilings. Retained source remains
`47796acb` from section16. Replacement96072 and tail366090 still require reductions of18496
(about19.3%) and67919 (about18.6%) to reach77576 and298171. Acceptance remains4/6.

### One tested change

Frozen0362 map Wasm `5fcff73d` reads page_count at offset16/length8 (call0x1f703) and initial
dir_len at offset48/length8 (call0x1fb57). Both call verified import0, ic0.stable64_read.
These are compiled call sites, not isolated prices; neither baseline Wasm nor IC benchmark was
rebuilt or rerun.

Candidate `98ecf8a1` swapped dir_len48→24 and free_head24→48, then read page_count and dir_len
in one16B call. Existing EditPlan held one extra initial u64, reusable only while dirty ranges
were empty. Its existing dir_len helper selected the snapshot or current probe and retained one
DIR_CAP guard. All three callers—lookup, split_page and remove_page—used that owner. The initial
planning inventory omitted the third caller; the complete source search included it before the
candidate was implemented. No validation moved before result reservation.

Header size, page/directory geometry, marker1 and public HeaderV1 were unchanged. The incompatible
slot trial followed the approved fresh-owning-bundle policy, with no migration or old-format
recognition. It introduced no public API, type, helper, persistent cache, flag, dependency or
FreeSpan change. Full-page/first-key/header checks, exact priors and publication remained intact.
Unlike rejected0356 prefetch, both words were mandatory initial metadata. No other candidate,
check deletion, batching, prior-value redesign, rekey or index replacement was bundled.

### Source-bound measurements and decision

| Metric | Retained0362 | Candidate0363 | Difference | Change | Original ceiling | Gate |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Missing64 |84008|83432|-576|-0.6856%|85672|PASS|
| Replace64 |96072|95368|-704|-0.7328%|77576|FAIL|
| Split/directory |29123|29164|+41|+0.1408%|30930|PASS|
| Tail pairs64 |366090|365002|-1088|-0.2972%|298171|FAIL|
| LARA total |819559|812749|-6810|-0.8309%|1527576|PASS|
| LARA named |818598|811788|-6810|-0.8319%|1526615|PASS|

**Decision: reject the entire candidate.** Split violates the required non-increase control even
though it stays below its original ceiling. Both target improvements and the other three controls
do not override that failure. No tolerance, aggregate waiver or post-measurement tuning was used.
Map total580226→577899 is context, not a seventh gate; the LARA scopes are overlapping, not additive.
All reported heap/stable page increases are0. Original5/2 fourteen-column rows, exact names,
named calls1, new status and empty deltas are verified; no results.yml was persisted.

### Correctness, compiled path and limits

Two semantic characterizations pass on unchanged production. A real staged2→1 directory shrink
changes subsequent routing while canonical bytes stay unchanged; forced result-scratch failure
precedes corrupt directory length. Independent constructor-traffic and64B-layout assertions then
fail on the old read/slot contract and pass on the candidate. Four existing fixtures still use
five native Memory/map constructions, with no new test function or IC fixture.

The stale-snapshot mutant still calls its probe and passes the earlier routing checks, then fails
at the real-shrink lookup with AddressOverflow. The early-validation mutant fails the exact
Scratch-at-None precedence assertion. Each mutant is archived and the candidate restored before
final validation. Candidate format, strict map lib/tests and LARA lib lint,30 map units,5 doctests
(1+4),27 FreeSpan tests, both release builds and both original IC filters pass as executions.
Existing GAP-LARA-TEST-CLIPPY remains open, not rerun or waived. LARA includes unchanged pending
admission work, not clean-commit integration evidence.

Candidate prepare_image starts0x1f6dd. Its call0x1f764 reads offset16/length16. The pristine
backing path uses the cached word and branches at0x1fc3b past the8B reread at0x1fc4a to0x1fc57;
bound8192 remains at0x1fc5a. The conditional reread is still emitted: the removed work is on the
executed pristine path, not a global absence of read code. Prospective read0x1fb22 and bound
0x1fb32 remain. Split keeps corruption/DirectoryFull bounds0x1d923/0x1d93e before scan0x1d988.
No isolated instruction price, attribution of the41-instruction regression or peak-stack claim
is inferred. Fewer native/stable-memory calls did not produce the required whole-operation result.

Lizard1.24.0 (`--csv -i -1`) measures nine touched functions: CCN66→71/NLOC521→625, maximum21
unchanged. Whole-file CCN478→483/NLOC3923→4027,148 functions and maximum32 unchanged. Production
adds11 NLOC and one branch; tests add93 NLOC. Only private dir_len gains self; no new five-plus
signature or abstraction appears. Candidate map/LARA Wasm grow182/363B to743412/5064795B. None of
these complexity/size changes is a speed claim or a reason to relax the failed control.

### Restoration

The entire candidate, including Rustdoc and test additions, is removed. Live lib is byte-identical
to HEAD0362:174449B/SHA256 `47796acb7e26ecdd138570922702d12968bd5c52983e89dfc745f436ff1b79e5`.
The lib diff is empty; ADR0088 and all unrelated pending work remain unchanged. There is no0363
layout adoption or new fresh-state requirement. Validation above belongs to the archived candidate;
restoration is proved by exact bytes, not a relabeled or rerun baseline validation.

Archived candidate lib178863B/SHA256 `98ecf8a104192afc7461e7d5e767747b2e6cc1d172b11639728b9f2287828d79`;
map743412B/SHA256 `390097ef26b39c0e25163a424f32ca325498cdd29b4327a19d0862f18752e42d`;
LARA5064795B/SHA256 `8cdd2f5c267eaf7892873e7149c1735be1c3639c3d68af4189492881b2de9a03`.
Map Wasm was frozen before LARA rebuilt its default-only map dependency; that linked rlib is archived.

Evidence: `$GLEAPH_EVIDENCE/adr0097/header-pair-read-0363/` contains planning/
approved input pins, characterization/red/candidate/mutant sources and transcripts, source/lock/
Cargo/Wasm bindings, original-filter CSVs, codegen/Lizard review, rejection and full-restoration proof.
No second candidate, baseline Wasm rebuild/IC rerun, benchmark persistence, commit or push occurred.

## 18. Shared edit-pipeline static survey

**Status: research complete; no implementation candidate selected (Plan0364).** This is static
inspection of retained0362, not an optimization trial, runtime profile or architectural rejection.
The inspected source is `47796acb`; map Wasm is743230B/`5fcff73d`. Both original source bindings
and map/LARA CSVs were checked without rebuilding or executing either artifact. Replacement still
needs18496 fewer instructions (19.25% of96072); tail67919 (18.55% of366090). Acceptance remains4/6.
Nothing here prices a helper, predicts19% savings, or establishes that further improvement is impossible.

### Ownership and complete common path

The [map source](../../crates/ic-stable-paged-ordered-map/src/lib.rs) has one scalar/batch engine:
`prepare_edits`/`scalar_edit` (1512/1519) construct a caller-owned EditImage, `prepare_image` (1528)
constructs EditPlan, reserves results, applies ordered intents, stores priors and admits backing.
Only `publish_image` (1607) writes canonical bytes. Discard/drop releases private scratch.

Scratch (339–428) owns representation, capacity, fallible growth and reserved writes for all three
consumers: results, encoded bytes and descriptors. EditPlan::reserve (750) reserves bytes before
ranges; EditImage::cover (512) owns interval composition, packed bytes and descriptor rebasing.
EditPlan::lookup (1021) owns selected-page validation and position; apply_edit (1080) owns the
operation-specific prior/length reads and mutations. These are different responsibilities, not
multiple editors that should be merged. No duplicated domain-validation owner was found in this
inspected common path. A capacity reservation, a checked live slice and selected-page geometry
prove different facts; their proximity is not evidence that one can be deleted.

On valid backing, nonempty preparations perform one backing-size query and one initial page-count
read. Result reservation precedes lookup; the basic mutating scalars reserve bytes/ranges after
their lookup/prior/length checks. They fit inline but still execute fitting-capacity decisions. Admission compares
required_end with backed_end and skips growth. Publication and conditional scratch destruction
remain in the scalar call; the public batch retains the same image across its ordered intents.

### Which work the fixed shapes actually exercise

Source: [original fixture](../../crates/ic-stable-paged-ordered-map/src/bench.rs),120–181;
`scalar_edit_work_contract`,2346–2493; `apply_edit`,1080–1235. Counts below describe one fresh-image
scalar call, not one64-operation scope. Existing native traffic assertions are inspected, not rerun.

| Shape | Backing reads / bytes | Mutation-time image reads after lookup | Byte/range reservation request | Positive cover/write emissions | Published ranges / bytes | Previously staged payload relocated by cover |
| --- | ---: | --- | --- | ---: | ---: | ---: |
| Replace64's one replacement |7 /96|Prior value|8 bytes,1 range|1, empty-image case|1 /8|0B|
| Missing64's one absent remove |9 /104|None|None|0|0 /0|0B|
| Tail insertion, page remains nonfull |10 /112|Map length|26 bytes,3 ranges|3: empty, then two disjoint insertions|3 /26|0B|
| Tail removal, page remains nonempty |11 /120|Prior value and map length|10 bytes,2 ranges|2: empty, then one disjoint insertion|2 /10|2B|

Replacement has no range merge, old-payload shift or normalization of an existing dirty region.
Tail insertion writes map length, page length and the new entry in increasing address order. Its
nonempty cover calls still run interval/bounds/descriptor work, but packed suffixes moved have
length0. Tail removal writes page length before map length: its only old packed payload is the2B
page length, moved behind the8B map length, with one descriptor shifted/rebased. Neither tail arm
shifts map entries or clears the removed entry. Thus a copy/normalization redesign must not be sold
as removing large payload movement from these two gates.

The split control is different: one actual split of64 entries plus one insertion; overflowing
byte/range scratch, a right-source snapshot, left/right covers, neighbor and directory changes.
`split_page` (816) is already out of line and preserves DirectoryFull-before-scan. Its full-page
admission rule is not interchangeable with the highest dirty address. Any shared-storage change
must preserve this path and the LARA consumers, not optimize only the four small shapes.

Missing is a useful zero-staging control, **not a subtractable fixed-cost calibration**. Its key129
selects the second page and a different search trace from replacement key64 on the first page;
even their read counts differ. `replace − missing` is not the cost of staging/publication and does
not establish a lower bound on attainable replacement cost. Tail scope contains128 separate scalar
calls, not one batch; amortizing them would change the original measured contract.

### Frozen Wasm corroboration

E=`$GLEAPH_EVIDENCE/adr0097/edit-pipeline-survey-0364/`.
`codegen/index.json` binds complete function excerpts to the frozen Wasm and reused disassembly
SHA256 `ecdb2253a4e354fa38d1f966c3ac092c0ca78c63cd6553ca5625fddf11f83b5f`.
Function identity/order is checked against the binary CODE entries; demangled names alone are not
unique, and LLVM's CODE section heading is not a function. Imports0/1/2/3 decode to
stable64_read/size/grow/write. Addresses below belong only to this artifact.

| Phase | Emitted evidence | Consequence for the proposed simplifications |
| --- | --- | --- |
| Construct/enter | scalar0x17d5; initialization0x17ee–0x1851; one-element Edit copy0x1855–0x1868; call prepare0x1886 | Caller owns the image; the copied input is24B, not a returned whole EditImage. Initialization is real, but deferred materialization already failed0350 |
| Plan/results | size0x1f6d2, page-count read0x1f703; inline result-fit branch0x1f7a5–0x1f7b2 | Setup exists once per call; fitting scalar results bypass allocation. Input specialization already failed0349 |
| Selection | backing lookup read0x1fb57; prospective lookup read0x1fa55 | The two emitted reader paths are alternatives, not two lookups per intent; validation remains single-owned in source |
| Mutation reads | replace prior0x20020; insertion length0x20060; removal prior/length0x2010d/0x2012d call EditImage::read | The adapter is still called on a fresh image. Its empty partition skips the search loop; disjoint backed read reaches import0 at0x139b |
| Byte/range reserve | EditPlan::reserve0x1df07; inline fits0x1dfa0–0x1dfb3 and0x1e1bf–0x1e1d1 | Two independent buffers have fitting checks; allocator calls elsewhere in this function are skipped by the four inline scalar shapes |
| First write | write0x1cb2a; empty-image branch0x1cbcd; byte resize0x1ccb4–0x1ccfa; inline descriptor append0x1cdc7–0x1cdfa | First-write fast case is already compiled. Replacement bypasses the general normalizer; its remaining representation checks are not evidence of relocation |
| Priors/admission | inline prior store0x216ae–0x216ce; required/backed comparison0x21724–0x21732 | Results are stored before one final admission check; basic paths skip grow0x2174f |
| Publish | call0x1981; range dispatch0x1c9ec–0x1ca1a; byte dispatch0x1ca30–0x1caa3 before loops0x1ca48/0x1caa5 | Byte Inline/Heap selection is already outside the range loops; there is no per-range representation match to hoist. Per-range slice bounds and writes remain |
| Cleanup | inline success skips deallocations via0x1997/0x19be and the prior-state branch0x19e2 | Drop is conditional state inspection, not unconditional allocation/deallocation on the basic scalar paths |

Lookup/apply/admission are already incorporated into prepare_image in this artifact; cover is
incorporated into write. Merely merging source helpers does not remove a demonstrated call chain.
The scalar/preparation stack adjustments are208/160B, respectively—not native object sizes,
peak call-chain stack, runtime costs or an argument for another scalar implementation. Large
functions contain mutually exclusive structural/error paths; their text size or opcode total
cannot be assigned to every successful scalar call. The old observer-affected scopes remain
ineligible for exclusive-cost subtraction or savings forecasts.

### Alternatives screened, not implementation proposals

| Scale / alternative | Work it could remove | Why it is not selected |
| --- | --- | --- |
| Minimum: carry the existing per-intent reader choice through prior/length reads | Some fresh-image adapter dispatch: one read for replace and three per tail pair | No stable-memory read disappears; empty-image search/overlay loops are already skipped. This is a small unpriced read-path change, not evidence for a structural19% opportunity. Dirty batches and unbacked-page behavior must still use current prospective bytes |
| Internal storage redesign: one Inline/Heap state for dirty bytes and descriptors | Some independent representation selections in reserve/read/write/drop | Their mixed states are legitimate: inserting130 into the97-key fixture at second-page position1 requests538 bytes but only3 ranges, requiring heap bytes and inline descriptors. Promoting both introduces an additional buffer allocation; publish already hoists selection. No measured attribution justifies this new coupling |
| Internal phase redesign: defer packing/coalescing until preparation ends | Repeated packed-byte relocation and intermediate descriptor maintenance | Replacement relocates0B; a tail pair relocates2B. Ordered prospective reads still need overlap composition, and an append history can grow with repeated writes rather than dirty bytes. No bounded replacement representation with a demonstrated common benefit has been established |
| Internal control-flow split: outline cold normalization/structural paths | Possibly reduce common emitted code/local state | Split is already outlined; code size is not executed cost. Moving more functions has no current causal evidence, and0349 shows a smaller scalar frame can still regress all controls |
| Larger persisted/index redesign, or FreeSpan grouping | Large-directory/interior movement or allocator operation counts | Fixed scalar directory has2 entries; those changes do not establish a benefit for value replacement and zero-entry-shift tail calls. Layout/policy/owner-integration decisions remain separate; they cannot waive the scalar gates |

The first row is a real remaining source distinction, not a newly approved candidate or a proved
bottleneck. This survey does not resume read-path micro-trials under a structural label. The joint
storage and delayed-packing rows likewise are not specifications for new types, modes or logs.

Architecture assessment:

- **Encapsulation:** keep image storage, lookup and publication private to the generic map;
  FreeSpan receives no geometry, raw buffers or unchecked addresses.
- **Separation of concerns:** read selection, operation semantics, scratch capacity, interval
  composition and publication have existing owners. No new phase/controller is needed merely
  because these functions appear in a call graph.
- **Invariants:** preserve exact prior values, result-scratch-before-edit failure order, the
  selected-page checks, full-page admission, inline limits and no canonical write before ready.
  Reservation fit and reserved-write bounds are not duplicate domain validation.
- **Consistency:** staged bytes remain authoritative after prior intents; jointly tagged buffers
  or deferred normalization would add transition/overlap obligations, not eliminate them for free.
- **Fitness:** no inspected alternative yet ties a proportionate change to removable work common
  to both failing shapes. A larger representation is not justified by the size of the desired gain.

### Decision and missing evidence

**Stop before implementation.** No0365 candidate is selected, no rejected trial is reopened, and
there is no reason to change ADR0088 or the retained layout. The current result is narrower than
“optimization exhausted”: it rules out several proposed explanations (large basic-path payload copies,
per-range publication dispatch, unconditional map-owned scalar heap work, repeated execution of both lookup
branches) and exposes the remaining adapter/representation bookkeeping without pricing it.

Before another implementation experiment, require either a concrete contract-preserving design
that removes substantive work on both target shapes, or current-source-bound cost evidence that
makes one such change worth its complexity. A new diagnostic would need separate approval and an
observer-effect argument; the old phase probes, unrelated workloads and baseline remeasurement are
not substitutes. This slice does not select or run a diagnostic protocol.

Only this research addition, its ignored plan and external evidence change. No Rust format/lint/
test/build, PocketIC, canbench, runtime tracing or Lizard was run; there is no new performance result.
GAP-LARA-TEST-CLIPPY stays open. All original fixtures, source/artifacts, layout and unrelated pending
work remain fixed. Evidence contains input pins, binary-bound excerpts, CSV arithmetic, exact scope
checks and draft/final plan validation. No benchmark persistence, staging, commit or push occurred.
