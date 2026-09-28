# ADR0097 performance architecture research (whole map-edit / FreeSpan execution)

Status: **Experimental — research and bounded implementation checkpoints, not ADR0097 acceptance.**
Sections0–10 preserve their named source epochs: their current-source wording, measurements and
hypotheses apply to those checkpoints, not the live implementation. The latest retained map source
is `4e5456e1` (Plan0358), described in [section11](#11-single-reservation-owner-retained).
Original acceptance remains3/6; unimplemented proposals remain proposals.

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

Evidence root: `/Users/yota/.local/share/gleaph/evidence/adr0097/ng-direct-s555`.
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

Evidence: `/Users/yota/.local/share/gleaph/evidence/adr0097/replace-scopes-0354/`.
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

Evidence: `/Users/yota/.local/share/gleaph/evidence/adr0097/lookup-scopes-0355/`.
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

Evidence: `/Users/yota/.local/share/gleaph/evidence/adr0097/directory-pair-0356/`.
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

Evidence: `/Users/yota/.local/share/gleaph/evidence/adr0097/first-cover-0357/`.
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

Evidence: `/Users/yota/.local/share/gleaph/evidence/adr0097/reservation-owner-0358/`.
Retained lib165868B/SHA256 `4e5456e152e8ddba352fd52d86f721bdf91aa234c01c42c429d2bc90d7f0c434`;
frozen map742773B/SHA256 `abad51cdadd3f2aaed4e59e9ac2c0010f11f5948c7070733350c8ba5a798384d`;
frozen LARA5063482B/SHA256 `cdcd861a113f7cee481406fec51e1e3e6517483f54246075820d7ecbe4100de7`.
Cargo records bind the linked default-only map rlib as well as both Wasm outputs. Source snapshots,
`mutation.json`, `measurement.json`, native streams, `inspect/` and `completion/receipt.json` bind
the retained decision and exact scope. No second optimization, benchmark sweep, commit or push is
part of this experiment.
