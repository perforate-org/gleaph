# Auxiliary index redesign research — tombstone distribution, counterpart mapping, ordered property delivery

Date: 2026-09-06 05:12:03 UTC +0000 (OS anchor; all external claims verified against cited sources on this date).
Last updated: 2026-09-07 01:28 UTC — §5 added: design revision after the Plan 0336 gate failure and
design review. Candidate A evolved from "adaptive bitmap" to the minimal header-count design
(Level 1); the byte-slab sidecar variant is rejected on single-role discipline; Level 2 is deferred
behind measured evidence. No design contract changed yet — the header-count field and its ADR are
Plan 0337 scope.
Status: **Research notes** — no implemented behavior, no design-contract change. Feeds three open
workstreams: the GAP-2026-07-25-002 revisit (tombstone-aware OFFSET acceleration), the ADR 0048
reserved adaptive-accelerator follow-up (counterpart lookup), and edge-side ordered delivery
(ADR 0081 Slice B/C scope).

## Purpose

Re-examine the design space for three candidate auxiliary indexes in the graph canister from
current (2024–2026) research and production-system evidence, before committing to an ADR:

1. **Compressed tombstone distribution** per edge bucket, accelerating tombstone override
   inserts and `TraversalWindow` OFFSET resolution. Coarse (block-granular) summaries are
   acceptable when the LARA scan remains the verification path; the open question is the
   balance between execution cost and persisted bytes.
2. **Compressed forward/reverse correspondence** mapping, reducing persisted bytes while
   relying on CounterpartScan as the exact fallback.
3. **Property-ordered indexes** for vertices and edges, and how much of that surface the
   existing graph-index canister already covers.

Method: multi-angle web research against primary sources (peer-reviewed papers, merged
production PRs, vendor engineering blogs), followed by mapping onto the current Gleaph
contracts in `design/implementation-gaps.md`, `design/adr/0048`, `design/adr/0050`,
`design/adr/0052`, `design/adr/0081`, `design/adr/0088`, and `design/storage/lara.md`.

## Non-goals

- Choosing a final design or opening an ADR from this document.
- Any code, stable-layout, or wire change.
- Re-measuring Gleaph benchmarks; all performance numbers cited here are external evidence
  from the cited systems, not substitutions for canbench.

---

## 1. Tombstone distribution / OFFSET acceleration

### 1.1 Workload-adaptive dynamic rank/select (strongest new finding)

- **Navarro, "Practical Adaptive Dynamic Bitvectors", Software: Practice and Experience
  (2025)** — <https://users.dcc.uchile.cl/~gnavarro/ps/spe25.pdf>.
  A dynamic bitvector whose cost adapts to the workload query ratio `q` (= queries per
  update): amortized `O(log(n/q))` for both updates and rank/select, space ≤ `(1+ε)·n` bits
  (measured ≤ 1.5n with ε = 0.05). Measured behavior:
  - Optimal leaf size **b = 2¹³ = 8192 bits** — the same order as LARA's 4,096-slot buckets.
  - **Bursty (batched) updates and boundary-concentrated updates are substantially cheaper**
    than uniform updates — matching Gleaph's batch-mutation path.
  - With rare updates (1/q ≤ 10⁻⁴) the structure is almost entirely static leaves and query
    time approaches the static bitvector baseline.
- **"Succinct Dynamic Rank/Select: Bypassing the Tree-Structure Bottleneck", SODA 2026**
  (arXiv 2510.19175) — removes the tree-traversal bottleneck theoretically; not yet a practical
  library, but raises the achievable update/query frontier.
- **SPIDER (SEA 2024)** and **DPR (DCC 2022)** — practical static/dynamic rank/select
  implementations; SPIDER is the fast static build path, DPR the fast fully-dynamic one.

**Implication for GAP-2026-07-25-002.** The 2026-08-23 rejection rationale was a fixed
crossover: `Q_upper = 831,216,051 / (59,210 − 27,095) ≈ 25,882` queries, where the numerator is
the one-time maintenance drain and the denominator is the per-query saving of the best
query-only candidate (two-level 32×32 block counts). That analysis priced a structure whose
maintenance cost is workload-independent. A `q`-adaptive compressed bitmap inverts this: buckets
with rare tombstone churn carry the summary at near-zero amortized maintenance, so the crossover
becomes per-bucket and churn-dependent instead of global. This reopens the gap with a different
shape of candidate: a **bucket-local compressed live bitmap (rank/select) maintained as derived
metadata**, with the LARA scan retained as the verification authority (ADR 0050's fail-closed
rules: never change `BucketEntryPosition` identity, never skip a live edge on an unverified
summary).

### 1.2 Block-summary granularity is the dominant knob

- **Lucene PR #16431 (merged 2026-08-22, Lucene 11.0): Block-Max WAND via DocValuesSkipper** —
  <https://github.com/apache/lucene/pull/16431>. Production evidence that **skip-block width,
  not the summary family, decides the gain**: 4096-doc blocks +11%, 1024-doc +97%, 512-doc
  +202%, 128-doc **+422%** QPS on a 10M-doc top-100 workload. Same block-max family as
  Plan 0283's fixed/two-level counters, but with granularity treated as a first-class
  parameter (default 4096 was judged too coarse).

**Implication.** Plan 0283 measured 32-slot and 32×32 two-level counts. The Lucene data says
finer granularity (or adaptive granularity) may move the query-side numbers substantially
before any structural redesign is considered.

### 1.3 Reactive maintenance instead of persistent metadata

- **RocksDB PR #13523 (merged 2025-04): trigger memtable flush based on hidden entries
  scanned** — <https://github.com/facebook/rocksdb/pull/13523>. During `Seek`/`Next`, the
  iterator counts tombstones/overwritten entries passed over; at a threshold the memtable is
  marked flush-eligible. `seekrandomwhilewriting` (10M range tombstones): 18.5 µs → 4.9 µs/op
  (**3.8×**) with more frequent flushes, no throughput regression. Follow-up PR #13593
  generalizes to average scan-cost triggering.
- **Acheron (SIGMOD 2023)** — <https://cs-people.bu.edu/mathan/publications/sigmod23-zhu.pdf> —
  tombstone lifecycle in LSM trees; FADE achieves timely physical deletion of obsolete entries
  without full-tree compaction (same philosophy as ADR 0088's tree-mode demotion).
- **Delta Lake deletion vectors** — row-level deletes persisted as per-file compressed Roaring
  bitmaps that scans consult; the industry-standard form of a persistent "tombstone
  distribution", but there the bitmap is first-class data, not derived metadata.

**Implication.** A third design pole exists between "exact sparse scan" and "persistent skip
structure": **track tombstones encountered during traversal windows (in-memory counters) and
invoke the existing maintenance/compaction machinery workload-adaptively**, with zero new
persisted layout. This matches the observation in GAP-2026-07-25-002 that compaction restores
the query to 27,095 instructions; the gap is only that compaction today is not
tombstone-pressure-driven.

### 1.4 Candidate space after research

| Candidate | Persisted bytes | Query gain | Maintenance cost | Notes |
|---|---|---|---|---|
| A. q-adaptive compressed live bitmap per bucket | ~n bits, density-proportional | large; near-static for churn-light buckets | amortized O(log(n/q)); batch-friendly | new derived-state contract needed |
| B. Finer/adaptive block counts | small, per-block | large per Lucene evidence | per-update counters | Plan 0283 family, finer than 32 |
| C. Reactive traversal counters → maintenance trigger | **0** | indirect (post-maintenance) | none persisted; moves cost to compaction | RocksDB #13523 analog; reuses existing compaction owner |
| D. Status quo (exact scan + compaction) | 0 | — | existing | current authority |

Any candidate remains subject to the integrated crossover methodology recorded in
GAP-2026-07-25-002 (query + mutation + update + validation + framing + repair costs).

---

## 2. Forward/reverse counterpart mapping

### 2.1 Production graph engines do not persist a counterpart index

- **LSMGraph (SIGMOD 2024)** — <https://arxiv.org/html/2411.06392> — multi-level CSR with
  per-vertex multi-level position index; per-edge tombstone markers + timestamps; deletions
  degrade due to existence-lookup I/O. Storage 45% of LiveGraph. The derived index is kept
  reconstructible from CSR edge offsets — same derived-state philosophy as Gleaph.
- **BACH (VLDB 2025)** — <https://www.vldb.org/pvldb/vol18/p1509-miao.pdf> — bridges adjacency
  list and CSR via LSM-trees for hybrid transactional/analytical workloads.
- **Teseo (VLDB 2021)** — <http://vldb.org/pvldb/vol14/p1053-leo.pdf> — the closest structural
  relative of LARA: sparse-array segments with gaps, density-threshold window rebalance,
  ROS/WOS dual formats (read-optimized vs write-optimized), **no rebalance on delete —
  periodic merge reclaims**, the same hysteresis shape as ADR 0088's demotion.
- **k2-trees** (and the dynamic variant, Navarro 2021) can answer both orientations from one
  compressed structure, but their update costs and their single-order semantics conflict with
  LARA's independent forward/reverse ordering contracts.

**Implication.** "0 bytes per edge of counterpart metadata" (ADR 0048) remains the industry
direction; no production system studied here materializes a forward/reverse mapping. An
accelerator must therefore justify itself per-workload, exactly as ADR 0048's follow-up
conditions state.

### 2.2 Negative finding against ADR 0048 reserved candidate 1 (target-aware block summaries)

- **DuckDB, "Sorting on Insert for Fast Selective Queries" (2025-05)** —
  <https://duckdb.org/2025/05/14/sorting-for-fast-selective-queries>. Zone maps (min/max) are
  selective **only when the data is sorted on the filter key**; unsorted data produces wide
  ranges that prune nothing. Sorting is maintained by bulk insert + periodic re-sort, never
  per-row.

**Implication.** LARA buckets preserve insertion order (`ORDER BY INSERTION`, ADR 0052), so
per-block target min/max summaries would be non-selective for mixed-target buckets. ADR 0048
candidate 1 is only viable with (a) block-level target membership filters (order-independent,
larger than min/max) or (b) target clustering for hot relations (candidate 3 territory). This
constraint should be recorded when the ADR 0048 follow-up is drafted.

### 2.3 Compressed-mapping direction

`design/storage/lara.md`'s logical model already encodes `reverse_slot − forward_slot`
residuals in 8/16-bit modes. Residual/gap coding connects naturally to **Elias-Fano rank/select
(SEA 2025)** and the dynamic-integer-set bounds ADR 0048 already cites (arXiv 1408.3045) — a
density-proportional sidecar mapping `(target, PairOrdinal) → occurrence` remains the most
promising persisted-metadata shape, with CounterpartScan as exact fallback.

---

## 3. Property-ordered indexes (vertex and edge)

### 3.1 Physical property-sorted adjacency is a rejected direction industry-wide

- **DuckDB (2025-05)**: sort at bulk-insert time + periodic re-sort; small per-row inserts
  destroy sort quality. Confirms ADR 0081's rejection of physical clustering (alternative 3).
- **Teseo / GraphOne / LiveGraph**: none stores adjacency physically sorted by property;
  property ordering is delegated to secondary indexes.

**Implication.** The hypothesis that the existing graph-index canister covers the ordered-index
surface matches industry practice; no LARA-side physical property ordering should be designed.

### 3.2 Index-backed ORDER BY — production state 2025–2026

- **Memgraph PR #3950**: "Eliminate ORDER BY when index scan provides ascending order" —
  identical optimization to ADR 0081 Slice A (vertex-side).
- **Memgraph PR #3996**: reverse label property index — DESC delivery served by a reverse
  posting structure rather than query-time sort; an implemented precedent for ADR 0081
  Slice B (DESC).
- **Memgraph PR #4336**: correctness fixes for **edge IN-list, ORDER BY elimination, and
  nested edge property filters** — the same edge IN-list gap Gleaph records in
  `design/implementation-gaps.md` (edge `indexed_equality` stays single-value; IN lists scan
  incident edges) was found and fixed in a production Cypher engine.
- **Neo4j issue #13417**: index-ordered scans still require a presence-guaranteeing predicate —
  the same eligibility constraint as ADR 0081's anchor requirement.

### 3.3 Learned indexes — not currently applicable

PGM-index (fully-dynamic compressed), UpLIF (2024), LOFT (EuroSys 2025), BLI (2025),
PIMLex (FAST 2025), and the VLDB 2025 analysis "Why Are Learned Indexes So Effective but
Sometimes Ineffective?" show that updatable learned indexes converge on learned-model +
tree-leaf hybrids whose update/rebuild obligations are heavy, and whose effectiveness depends
on key distribution and sortedness. Gleaph's postings are already value-ordered by the sortable
index key with paginated range export (`lookup_range_page`, `lookup_edge_range_page`), and the
remaining ordered-delivery cost sits in the federation merge — outside a learned model's reach.
Adoption is not justified now.

**Remaining design surface for ADR 0081 Slice B/C:** DESC (reverse posting delivery or backward
iteration), edge-side ordered delivery through edge postings, and the edge IN-list probe
(implemented precedent: Memgraph PR #4336).

---

## 4. §1 narrowed comparison — candidate A (adaptive bitmap) vs candidate C (reactive counters)

Narrowed 2026-09-06 05:55 UTC with the graph-impl implementation-side report. Sequencing
**C first, then A only if C's decision function fails**; the decision function is fixed before
any measurement.

### 4.1 Implementation-side constraints (graph-impl report, code-verified)

**Candidate A landing points (slab buckets only):**

- `LabelBucket` is a fixed 29-byte descriptor (`record.rs:29`); a bitmap root does not fit.
  The natural home is a **sidecar span on the byte-slab allocator** via the same
  reserve / stepped-fill / publish path Plan 0320's `MaterializeInlinePropertyStreamV1`
  already uses — no descriptor widening, no ADR 0007 layout change for a first measurement.
- Extent 8,192 ⇒ 1 KiB raw bitmap; Navarro's optimal leaf b = 8192 bits maps exactly to
  one bucket, so the structure degenerates to **in-leaf popcount rank + a small superblock
  summary**; a two-level tree is unnecessary. Candidate B is absorbed as A's granularity
  parameter (as anticipated in the first draft of this document).
- Tree buckets (LTB) are out of scope: Plan 0283 measured slab buckets only, and tree-mode
  tombstone reclamation is owned by the ADR 0088 demotion/rewrite path. Any window jump must
  follow the existing single tree-mode dispatch point (`remove.rs` Plan 0318 comment).
- The update contract must hook **every** ADR 0052 block-local swap-compaction and overflow
  unlink path (`remove_edge_at_slot_with_move` survivor shifts); a missed hook silently
  corrupts derived metadata and raises fail-closed verification cost.
- Corruption handling must be a **degrade-to-exact-scan fallback**, not `LabeledOperationError`
  (ADR 0050's fail-closed rules, single derived-state owner, `BucketEntryPosition` identity
  unchanged).

**Candidate C boundary analysis:**

- The no-await boundary is not the obstacle (LARA is synchronous single-message execution;
  ADR 0029/0045 are unaffected). The obstacle is the **read/mutation boundary**: `visit_edges*`
  are `&self` read APIs, so durable worklist enqueue inside traversal would violate it.
- Workable shape: **ephemeral in-heap tombstone-pressure counters** (lost on upgrade;
  fail-open is acceptable for a non-correctness heuristic) + enqueue executed at the
  **mutation boundary or the deferred maintenance timer drain** (precedents: tree-mode
  demotion check after a successful remove at `remove.rs:328`; maintenance
  `CompactLabelBucketVertexSegmentV1` enqueue + `DeferredConfig::leaf_dirty_density = 0.85`
  dirtying in `deferred.rs`). This is structurally the RocksDB #13523 analog — its counters
  also live in in-memory memtable stats, with only the enqueue result durable.
- Counters stay LARA-internal; they are not exposed through the Graph API surface.

### 4.2 Measurement plan (increments over the Plan 0283 fixture)

The existing 0283 fixture shape and preflight (window parity, boundary offsets 31/32/33) are
reused as-is.

Shared (both candidates):

1. **Interleaved workload harness** — queries and mutations interleave inside one closed loop
   with q (queries per update) parameterized. 0283 measured phases separately (empty query,
   drain, post-restore query); the integrated crossover claim needs exactly this harness.
2. Churn axis at three points: churn-light (query-dominant), bursty batch, uniform updates.

Candidate A specific: update leg (summary update inside insert/delete, DirectoryRebuild-style
accounting on the production path), build/rebuild leg (bucket creation and post-compaction
summary construction), reopen leg (sidecar persistence + validation; FreeSpanStore reopen
thresholds are the threshold-setting precedent), plus fail-closed assertions (corrupted summary
degrades to exact scan, identical window results).

Candidate C specific: counter-accumulation delta on the existing 87.5% OFFSET-960 bench, and
**targeted bucket-local compaction cost — C's real unknown** (0283's 831M-instruction drain
was a whole vertex-span drain; C only triggers one bucket's compaction). No reopen leg.

Form: new `offset_workload_*` bench groups beside the existing `canbench --csv --hide-results
offset_live1024` family. For A, a non-zero stable-memory delta is the correct expectation.

### 4.3 Decision function (fixed before measurement)

Sequence C → A. Rationale (implementation-side, concurred by design review):

1. **Cost asymmetry**: C touches no persisted layout, descriptor, reopen contract, or
   corruption path and is fully reversible; A requires the derived-state contract designed
   before its numbers mean anything.
2. **Cheap falsifiability**: the §1.3 hypothesis (compaction already restores 27,095
   instructions; only tombstone-pressure triggering is missing) is directly testable on the
   existing drain machinery. If targeted bucket compaction shrinks the 831M global drain
   enough, A's ~n-bit derived metadata and its update/rebuild obligations likely never
   justify themselves.
3. **C generates A's inputs**: traversal counters measure the actual per-bucket, per-time
   tombstone-pressure distribution — the churn data A's q-adaptivity assumes. C-first
   establishes whether churn-light buckets even exist.
4. **Failure is informative**: if C never fires on query-heavy workloads, C degenerates to
   status quo (D), and that workload-dependence measurement is the evidence A would need.

Decision rule: define, before any run, the **materiality gate on cumulative overshoot** —
the tombstone-scan instructions accrued between C's trigger points. If the gate passes, C
stands and A is not opened. If it fails (indirect gain insufficient; C's cost between triggers
is status quo, unlike the immediate query-side gain Lucene #16431 attributes to fine
summaries), proceed to A with the fixture legs of §4.2 and the churn distribution C measured.

All three areas keep the established contracts: single derived-state owner, LARA scan as the
verification authority, and integrated-cost crossover evidence before any ADR.

#### Measured outcome (2026-09-06, Plan 0336)

Plan 0336 implemented the interleaved workload harness and the candidate C prototype in
`crates/ic-stable-lara/src/labeled/graph/traverse/bench_workload.rs` (bench-scope only; no
production path changed). The decision function below was applied with the fixed anchors
(ε = 0.10, `I_restored = 27,095`) and the declared C trigger threshold
`C_TRIGGER_THRESHOLD = 853,000` tombstone-observations (one compaction's worth of scan
overshoot at the 0283-era anchors). No number was retuned after measurement.

**Measured loop totals (2,048 window queries per loop; `canbench --csv --hide-results
offset_workload`):**

| Churn point | q | M | I_D(W) | I_C(W) | I_C / I_D | gate (I_C < 0.9·I_D) |
|---|---|---:|---:|---:|---:|---|
| churn-light | ≈102 | 20 | 277,762,489 | 1,241,855,332 | 4.47× | FAIL |
| bursty | s=80 | 160 | 303,233,375 | 1,386,750,801 | 4.57× | FAIL |
| uniform | ≈13 | 158 | 300,623,599 | 1,390,008,344 | 4.62× | FAIL |

**Decomposition terms:**

- `I_compact_local` (one targeted bucket compaction under tombstone pressure) =
  **944,957,827** instructions. Ratio to the 0283 global-drain total (831,216,051) = **1.137**;
  ratio to the 0283 per-item average (831,216,051 / 1,025 = 810,942) = **1,165×**. At this
  fixture the "global drain" and a "targeted bucket compaction" are the same single-bucket
  operation, so the plan's global-vs-targeted distinction collapses: one targeted compaction
  costs ≈ 1.14× the 0283 drain total.
- `I_counter_delta` (per-query counter accumulation) = 139,509 − 133,144 = **6,365**
  instructions/query (4.8% of a scan).
- `I_scan_total` ≈ 2,048 × 133,144 = 272,678,912; `N × I_restored` = 2,048 × 27,095 =
  55,490,560; **overshoot ≈ 217M per loop ≈ 106,045/query** (material).
- Per-query overshoot A's bitmap could reclaim = `scan_only − restored_query` =
  133,144 − 29,996 = **103,148** ≤ the 845,539 bound. ✓
- `K` (compactions fired per C loop) = **1** at every churn point (the declared threshold
  crosses once; the counter re-accumulates too slowly to fire again within 2,048 steps).

**Gate verdict: C fails the gate at all three churn points.** The failure is structural, not a
threshold artifact: one targeted compaction (≈ 945M) must be amortized against the overshoot
accumulated since the last trigger. At the current-code per-query overshoot (≈ 106K, post-0327
semantics — 8× smaller than the 0283-era 845K anchor), break-even is ≈ 8,900 scans per
compaction; even at the 0283-era overshoot it is ≈ 983 scans, and the 0.90 gate cannot pass at
break-even. The measured churn distribution (K = 1; counter crossing ≈ 119 scans; break-even
≈ 8,900 scans) shows the failure mode A addresses: material overshoot persists at every churn
point while C's trigger cannot fire often enough to reclaim it.

**Conclusion: C fails gate — A opens.** Per the fixed decision function, proceed to candidate A
with the §4.2 A-legs. The measured churn distribution (compaction-cost-dominated at every
point, not churn-light starvation) justifies prioritizing the A fixture legs in this order:

1. **update leg** — A's per-update summary maintenance must be ≪ 945M (the compaction cost C
   pays per trigger); this is the binding economic comparison.
2. **build/rebuild leg** — initial summary construction and post-compaction rebuild.
3. **fail-closed leg** — corrupted summary degrades to exact scan with identical window
   results.
4. **reopen leg** — sidecar persistence + validation (FreeSpanStore reopen thresholds).

Also recorded from this slice (context for the A measurement): the current sparse scan is
133,144 instructions (harness) / 133,587 (0283 family) vs the 0283-era 872,634 anchor — the
post-0327 tombstone-inclusive position semantics already reclaimed ≈ 6.5× of the original gap,
so the materiality baseline for any accelerator has moved. The restored-query control measures
29,996 (harness) vs the 27,095 anchor, so `I_restored` remains representative. The existing
`bench_t_off_*` family is unchanged (10/10 unchanged).

Also recorded as an observed constraint (not investigated further in this slice): the per-leaf
overflow log table (`DEFAULT_MAX_LOG_ENTRIES = 170`) caps reinserts before the inline log-fold
path runs; a bursty probe at 1,000 mutations returned `Store(CollectAllocationOverflow)` under
10-byte edges at extent 8,192. This bounded the workload-shape mutation volumes in the harness
(see the Plan 0336 report, §1/§5) and is a candidate for a future implementation-gap entry if
it reproduces on the production path.

### 4.4 Remaining areas (unchanged from the first draft)

2. **Area 2**: when drafting the ADR 0048 follow-up, record the DuckDB-derived constraint that
   target-aware min/max zone maps are incompatible with insertion-ordered buckets; evaluate
   membership-filter and hot-relation-only variants against CounterpartScan.
3. **Area 3**: no new storage; extend ADR 0081 toward Slice B (DESC) and edge-side ordered
   delivery, and treat the edge IN-list probe as a planner-scope gap.

---

## 5. Design revision (2026-09-07, post-0336) — Level 1 header-count, minimal role

After Plan 0336 measured candidate C's structural failure (§4.3), candidate A evolved through two
intermediate shapes before settling. This section records the accepted design direction and the
rejections; it supersedes the sidecar-bearing variants described earlier in this document.

### 5.1 Evolution and rejections

1. **Adaptive compressed live bitmap (§1.1/§1.4 candidate A, as originally framed)** — superseded.
   At LTB block scale (1,024 slots/block) the Navarro-class machinery is unnecessary; the block's
   own payload already carries per-slot liveness as the tombstone sentinel marker, so a bitmap
   would duplicate state the block already owns.
2. **Per-bucket sidecar span on the inline-property-bytes byte-slab (rejected)** — placing the
   directory as one more span of `FWD/REV_INLINE_PROPERTY_BYTES_SLAB` (graph MemoryId 10/25, the
   region holding edge INLINE property bytes for slab buckets) would give a canonical
   single-purpose region a second role: tree-mode invariant rewrite (`inline_property_bytes_offset
   / slab_slots = 0` repurposed as a sidecar descriptor), descriptor-field overloading, and
   compaction coupling. Rejected on single-role discipline (management-pane design decision,
   2026-09-07). Not revisited without new evidence.
3. **Accepted — Level 1: per-block tombstone count in the LTB block header (tree buckets).**
   A `u16` count at block-header offset 13-14 (inside the existing 16-byte header; stride 4,112
   unchanged; no store-header change). **No layout-version bump and no migration framing: the
   canister is not deployed, so the field is simply added to the current header and used; layout
   changes continue to require fresh state under the existing pre-production policy.** The count
   is the block's own accounting (not a second structure): mint initializes it to zero, and the
   tree-mode remove funnel maintains it inside the existing marker→descriptor compensation chain
   (`tree_mode_remove_edge_at_slot`, single funnel point). Level 1 is tree-mode-only — slab
   buckets have no internal blocks; their OFFSET overshoot is bounded by extent 4,096
   (post-ADR 0088 slab cap) and is decided by measurement, not by this design.

### 5.2 Contracts (Level 1)

- **SSOT:** the block's own marker bytes; the count is derived state stored in the same header,
  maintained only by the mutation funnel. No second source of edge identity; no
  `BucketEntryPosition` change; `visit_edges` output unchanged.
- **Verification:** fail-closed at use — the select/entry block scan recomputes the block's true
  live count and compares it with the header count; a mismatch is `LabeledOperationError`
  (block corruption class, ADR 0050), not a silent wrong window. Reopen performs no
  O(blocks) count validation (ADR 0088 §8 discipline unchanged). Bucket-level Σ check
  (Σ counts == `stored_slots − degree`) is confirmed during walks at zero extra cost.
- **Select path (Level 1):** OFFSET resolution walks root entries and reads one header per passed
  block, skipping fully-dead blocks without touching their slots. Scattered header reads bound the
  gain at ~5-8× on the dead-block component (page-charge floor: 1 page per passed block).
- **Deferred — Level 2 (contiguous per-bucket live-count directory in a dedicated single-role
  region):** escalates only on measured evidence — when the Plan 0337 fixtures show Level 1's
  integrated economics insufficient for the measured K distribution (blocks passed per OFFSET) and
  churn shape. Candidate shapes recorded for that gate: (i) global dense u16 array indexed by
  block id (minimal machinery; select page-efficiency degrades when compaction/reuse churn breaks
  block_id ≈ logical-order locality), (ii) per-bucket chunked pages (optimal access, heaviest
  machinery, ADR 0032-style). No byte-slab reuse.
- **Adjacent pay-off:** the header count is exactly the primitive the deferred ADR 0088
  follow-up `tree-mode-tombstone-reuse` needs (count > 0 → bounded in-block scan finds the
  reusable slot), without the "no spare `LabelBucket` field" problem.

### 5.3 Measured expectations (honest ladder)

| Regime / level | OFFSET select cost (1M edge · 50% tombstone · OFFSET ~10K) | Ratio |
|---|---:|---:|
| Status quo (slot walk) | hundreds of K instructions (2 pages + slot logic per passed block) | 1× |
| Level 1 (header count) | 1 header page per passed block | ~5-8× |
| Level 2 (contiguous directory) | 1-2 pages total | ~100×+ (reference) |

### 5.4 Documentation gaps surfaced by the region-layout review (out of Plan 0337 scope; record and fix separately)

#### Plan 0337 outcome (2026-09-07)

Plan 0337 implemented the production header-count field (`BlockHeader.tombstone_count: u16` at
offset 13-14, reserved shrunk to 1 byte at offset 15; `LAYOUT_VERSION` unchanged at 1; no
migration/compat code — the canister is not deployed and dev state is recreated under the
fresh-state policy), the mint init, the remove-funnel increment inside the existing marker →
count → descriptor compensation chain (with payload + count rollback on descriptor failure),
and the parity regression tests (`tree_remove_increments_block_header_tombstone_count`,
`tree_remove_idempotent_does_not_double_count`: per-block count == scanned markers, Σ ==
`stored_slots − degree`). Production read paths are unchanged; the counting walk is bench-scoped
(`bench_tree.rs`). Audited marker writers: compaction rewrite and promote transcription mint
fresh blocks (count 0 via mint init); batch `RunDestination::Tree` tail writes append live edges
(count unchanged); demotion emits slab (no tree blocks).

**Tree OFFSET baseline and Level-1 results (first tree-regime OFFSET measurement; 1M+1 stored
slots, depth-2, contiguous dead-prefix tombstones, OFFSET window 32, canbench grid reduced to
2 densities × 2 OFFSET points for budget — see deviations in the /tmp report):**

| Point | density | offset | I_D (exact walk) | I_L1 (header-count walk) | I_L1/I_D |
|---|---|---:|---:|---:|---:|
| 1 | 50% | 524,288 (first live) | 48,570,803 | 912,629 | 1.9% |
| 2 | 87.5% | 917,504 (first live) | 48,177,587 | 914,549 | 1.9% |
| 3 | 87.5% | 983,040 (live+65,536) | 48,112,051 | 914,869 | 1.9% |

- The exact walk is offset-INSENSITIVE in the tree regime (~48.2-48.6M at every OFFSET point) —
  the tree window walk resolves block ids from the root and does not walk the dead prefix as a
  slab scan does, so the overshoot is the per-block page charge across ~950 passed blocks, not
  prefix length. K (blocks passed) = leaf_count ≈ 1,025 for every point — far above the K ≥ 32
  page-charge threshold.
- Level 1 achieves ≈ **52.8× per-query gain** (I_L1/I_D ≈ 1.9%), clearing the ε = 0.10 gate at
  EVERY measured point (`I_L1 < I_D × 0.90`). The gain exceeds the §5.3 "honest ladder"
  expectation of ~5-8× because the exact tree walk re-reads block payloads for tombstone slots,
  while the Level-1 walk reads only the 16-byte header for fully-dead blocks and scans payloads
  only in the window-overlapping block.
- Count parity held on every measured query (per-block header count == scanned markers; Σ ==
  `stored_slots − degree`).

**Verdict: Level 1 adopted — ADR draft next.** The measured K distribution (K ≈ 1,025 ≫ 32) also
confirms Level 2's shape driver (page charges, not slot logic) is real, but Level 1 already
reclaims ~98% of the gap, so Level 2 escalation is NOT recorded as needed at this fixture scale.

**Slab re-anchor (extent 4,096 — the post-ADR-0088 legal slab ceiling; 0283 worst-case shape,
87.5% tombstones, OFFSET 960, LIMIT 32):** measured 133,370 instructions/query. Against the
extent-4,096 dense OFFSET control (~24,243 at the pre-0088 fixture), the worst-case overshoot is
≈ 109,127 instructions/query — above the declared 100,000 bar. **Slab regime: the status-quo
rule fails; a slab-side follow-up candidate (512 B bitmap class) is recorded as
deferred-with-evidence, NOT opened** (the slab overshoot is 1.2× the bar, an order of magnitude
below the tree-regime overshoot, and the extent-4,096 cap bounds the worst case; a dedicated
slab slice would need the 0283 two-level counts re-measured at the legal cap).

Existing bench families: `bench_t_off` 10/10 unchanged; `offset_workload` (0336) unchanged
within ε (all 10 values byte-identical to the 0336 report). All validation passed: cargo check,
clippy (-D warnings), fmt, `cargo test --lib header` (10 passed), `--lib tree_remove` (4 passed).

### 5.4 Documentation gaps surfaced by the region-layout review (out of Plan 0337 scope; record and fix separately)

1. `design/storage/stable-memory-inventory.md` does not yet record the ADR 0088 LTB regions —
   the code allocates `FWD_LTB` = graph MemoryId 53 and `REV_LTB` = 54
   (`crates/graph/src/facade/stable/memory.rs`), i.e. 55 graph regions (0-54), while the
   inventory status line still reads "54 regions, 0–53".
2. ADR 0088 §1 states a 64-page VMM bucket policy for the LTB regions, while the code's
   `GRAPH_MEMORY_MANAGER_POLICIES` lists `(FWD_LTB, 16)` / `(REV_LTB, 16)`. Verify which is
   intended and align the ADR or the code.
