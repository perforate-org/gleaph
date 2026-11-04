# Value-only folding lowers maintenance cost, but does not replace both-plane folding

**Status: Experimental — preserved pre-retirement-repair measurements; no automatic fold policy change.**

Retaining the edge run while folding values reduces maintenance instructions by **4.3–68.2%** in
these four dense fixtures. With no intervening writes, full property scans repay value-only folding
after **18–22 scans at width 8** and **4–5 at width 33**. The corresponding both-fold figures are
26–47 and 4–6. However, value-only reads remain slower than both-fold reads, and the eight-appends /
one-values-scan episode does **not** repay either maintenance operation in any case.

Value-only folding is therefore a candidate for property-first, read-heavy work, not a general
replacement. Width-8 hydrated scans favor both-plane folding, and the first append at width 8 is about
40–42% more expensive after value-only folding than without folding.
[0370 reclamation diagnosis](adr0097-value-prefix-reclamation.md) confirmed missing old-prefix
retirement and failure to recover it through later compaction. [0371 repairs the shared publication
boundary and remeasures these exports](adr0097-value-span-retirement.md). The measurements below are
preserved pre-repair observations; neither experiment establishes a repeated-fold capacity policy.
The later [0373 repeated-fold experiment](adr0097-repeated-fold-economics.md) measures alternating
append/fold/read totals and allocator reuse on repaired source. Its different operation sequence
is reported separately, without replacing these historical tables or choosing a policy threshold.

This extends [0368 fold economics](adr0097-fold-economics.md). All three representations are measured
on **one current Wasm**; old results are not used as a baseline. The original setup is unchanged,
but workload totals now include seven additional appends and a final values scan. They are not
comparable with 0368's shorter workload totals.

## One fixture, three representations

Widths are 8/33 bytes and initial degrees are 64/256. A single source vertex has a target label and
one peer edge, inserted after the first 32 target edges. This blocks value-prefix tail growth during setup.
Inputs are unique per slot, hole-free and below tree promotion. The original fixture constructor,
codec, allocation sizes, sixteen MemoryIds and insertion history are unchanged.

| Representation before reads | Target edge prefix / run used | Target value slab / run used |
| --- | --- | --- |
| Unfolded, degree N | 0 / N | 32 / N−32 |
| Values folded | 0 / N | N / 0 |
| Both folded | N / 0 | N / 0 |

`fold_bucket_values_run_into_slab` is the existing value mutation owner. Value-only arms do not call
edge compaction. Both-fold controls call `compact_vertex_edge_span_with_moves(src, 0)` and then the
same value fold. The value-only transition asserts unchanged vertex edge span, bucket edge extent,
run ID and run used length; only value allocation accounting may change. No logical slots move.

The common workload measures, in order:

1. Full ascending topology scan.
2. Full descending values scan with reusable scratch warmed outside timing.
3. Full descending hydrated scan.
4. First append (`append`).
5. Seven more appends (`append_tail`).
6. Full descending values scan at N+8 (`values_after_appends`), using the same scratch.

All construction, exact-result checks, snapshots and allocator probes are outside timed closures.
The scratch is warmed at N, not at N+8: any buffer growth needed by the final scan is included.
The reads are distinct operations, not three parts of a query. Appends use the existing
`insert_edge_skip_leaf_cascade` / Insertion experiment path, not the public cascading insertion API,
Graph/Router execution or sustained ingestion. No maintenance behavior in that path is disabled or changed.

## Lower maintenance cost does not imply the fastest read

All numbers below are **Wasm instructions**. Maintenance totals include their measurement wrappers.
The value-only fold is independently measured with the edge run present; it is not obtained by
subtracting the edge scope from the both-fold total.

| Width / initial degree | Both-fold total | Value-only total | Value-only scope |
| --- | ---: | ---: | ---: |
| 8 / 64 | 396,825 | 126,177 | 125,234 |
| 8 / 256 | 1,072,129 | 576,255 | 575,312 |
| 33 / 64 | 1,566,856 | 1,292,604 | 1,291,661 |
| 33 / 256 | 12,271,700 | 11,745,026 | 11,744,083 |

Read-only repayment is `ceil(fold_total / (unfolded_read − folded_read))`, only for positive savings
and the same full scan repeated on unchanged data. It is not a runtime trigger or an estimate for
arbitrary predicates, partial reads or an evolving graph.

| Width / degree | Values: unfolded | Values: value-only | Values: both folded | Scans to repay: value-only / both |
| --- | ---: | ---: | ---: | ---: |
| 8 / 64 | 24,352 | 18,502 | 15,847 | 22 / 47 |
| 8 / 256 | 97,442 | 65,197 | 55,582 | 18 / 26 |
| 33 / 64 | 326,228 | 24,419 | 20,740 | 5 / 6 |
| 33 / 256 | 3,370,331 | 82,176 | 75,162 | 4 / 4 |

| Width / degree | Hydrated: unfolded | Hydrated: value-only | Hydrated: both folded | Scans to repay: value-only / both |
| --- | ---: | ---: | ---: | ---: |
| 8 / 64 | 31,008 | 25,521 | 9,541 | 23 / 19 |
| 8 / 256 | 119,112 | 86,873 | 27,589 | 18 / 12 |
| 33 / 64 | 341,607 | 35,288 | 13,414 | 5 / 5 |
| 33 / 256 | 3,449,755 | 119,147 | 42,399 | 4 / 4 |

The code explains why a retained edge run is not equivalent to a fully dense slab. In
`traverse.rs`, `visit_edges_with_inline_property_impl` admits its dense bulk path only when
**both** run IDs are absent; value-only folding still uses the span iterator. Likewise,
`visit_out_inline_property_batches_for_label_next` routes the retained edge run through the hybrid
batch reader. That reader constructs ordered ordinal ranges even when the values now reside in
one slab. This experiment does not change those routing contracts or optimize away their work.

Topology instructions are exactly equal between unfolded and value-only states: 5,514 / 13,957
for width 8 at degrees 64/256, and 6,267 / 15,394 for width 33. Values-only maintenance has no
read-only topology break-even. The both-fold topology counts are 5,369 / 13,609 and 5,830 / 15,115.

## Eight writes and one read do not recover the upfront fold

The first append grows the existing edge run in the unfolded/value-only arms; the both-fold arm
creates a new small run. Unfolded values remain run-backed, while folded values extend their slab
tail. Retaining the edge run alone does not remove the width-8 slab-append penalty.

| Width / degree | First append: unfolded | First append: value-only | First append: both folded |
| --- | ---: | ---: | ---: |
| 8 / 64 | 12,842 | 18,000 | 17,456 |
| 8 / 256 | 12,785 | 18,213 | 17,456 |
| 33 / 64 | 42,297 | 18,092 | 17,548 |
| 33 / 256 | 47,327 | 19,065 | 18,441 |

The charged episode below is **fold total + first append + seven-more-appends + final values scan**
(or zero fold cost for unfolded). This is explicit accounting from separate scopes, not a separately
measured end-to-end export. It excludes the initial three read scopes and never treats the fold as free.

| Width / degree | Unfolded episode | Value-only episode, with fold | Both-fold episode, with fold |
| --- | ---: | ---: | ---: |
| 8 / 64 | 122,970 | 285,820 | 553,615 |
| 8 / 256 | 192,698 | 779,087 | 1,276,958 |
| 33 / 64 | 818,227 | 1,455,397 | 1,729,773 |
| 33 / 256 | 3,839,635 | 11,973,035 | 12,499,725 |

Folding reduces the post-write values scan cost, but one scan is not enough to pay for maintenance.
There is no automatic refold in this episode. Its final degrees are 72/264; it does not establish
long-run throughput, repeated-fold behavior, class-exhaustion behavior or a workload-independent threshold.

For completeness, the workload export totals below include all six scopes, exclude setup folds,
and are **not** the charged episode or the break-even denominator:

| Width / degree | Unfolded workload total | Value-only workload total | Both-fold workload total |
| --- | ---: | ---: | ---: |
| 8 / 64 | 190,105 | 215,484 | 193,758 |
| 8 / 256 | 429,242 | 374,892 | 307,820 |
| 33 / 64 | 1,498,452 | 234,800 | 208,934 |
| 33 / 256 | 10,681,148 | 450,759 | 366,734 |

## Value-run reuse is proven; memory shrink is not

Values-only folding releases a 264-byte values run at degree 64 or a 2,056-byte run at degree 256
(header and class capacity included), while retaining the live 260-byte or 1,028-byte edge run.
The shared untimed probe checks retained IDs/used lengths and never allocates or releases a live run.
For the released value class, allocation returns the exact old ID with unchanged arena high-water
and virtual/physical page counts, then releases it again. Both-fold callers still explicitly require
both run IDs to be absent before probing; their original release contract is preserved.

Values-only edge-slab declared capacity stays at 4,112 slots and its vertex span stays at 1 slot.
Both-fold controls grow declared capacity to 4,208/4,416 slots and the vertex span to 65/257 slots. Avoiding that
slab growth retains the edge run; these figures are not additive net memory savings.

Value slab capacity is identical between both-fold and value-only arms:

| Width / degree | Slab occupied tail: unfolded → folded → after eight appends | Declared capacity after eight appends |
| --- | ---: | ---: |
| 8 / 64 | 264 → 776 → 840 B | 4,096 B |
| 8 / 256 | 264 → 2,312 → 2,376 B | 4,096 B |
| 33 / 64 | 1,089 → 3,201 → 3,465 B | 4,096 B |
| 33 / 256 | 1,089 → 9,537 → 9,801 B | 9,801 B |

On this experiment's source, `ensure_bucket_inline_property_bytes_span` did not retire the old
32×W prefix to the free-span owner. Recorded value free bytes are zero; the old prefix is not counted
as reclaimed. Follow-up0370 confirmed the same loss after explicit compaction at degree64, widths8/33.
The repaired-source free-byte and instruction measurements are recorded separately in0371.

All 36 IC snapshots exactly match corresponding native snapshots. The native run retains 48 records
(40 unique width/degree/phase states), including first-append and edge-only states. `degree` in each
record is the initial degree. In every recorded phase the virtual regions sum to 32 pages and the
physical backing stays at 2,049 pages, using the same MemoryManager 128-page buckets described in 0368.
All 20 totals and 144 scopes also report zero heap/stable page increase. This is not peak allocation,
absence of temporary allocation, blob B-tree free-node accounting or physical memory shrink.

## Validation and reproducibility

Source: HEAD `24467b17b375b2a7bc462818ef278553a52993fc` plus the preserved 0368 child module and this
benchmark-only extension. The only changed Rust path is
`crates/ic-stable-lara/src/labeled/graph/traverse/bench/fold.rs`.

Evidence root (`E`): `$GLEAPH_EVIDENCE/adr0097/value-only-fold-0369`.
`before.json`, the before/current source snapshots, `extension.patch` and `protected.json` separate
this change from the 266 frozen dirty paths. All original 265 protected paths remain in that set;
the additional protected path is the parent bench module. Index and original Cargo.lock are unchanged.

- Wasm: 6,561,167 bytes; SHA256 `9c87b5fe8f708ea3f2edb0a0d2da56bf5dfb6a38d74d9160009981375be3a4e6`.
- Child source SHA256: `4f20795663c8a65581769b31a935c2edbb787800ecd4a93df092c75a25c793d9`.
- Complete 164-row `fold-results.csv` SHA256: `f47cb5e58db73f254fbabec8308ca3e3f4015841fe0587854416346fb10a1bd4`.
- Common lock SHA256: `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`;
  original restored lock: `15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`.
- `comparison.json` contains exact operation counts/deltas, repayment arithmetic and charged episodes;
  `capacity.json` contains every native/IC snapshot; `measured.json` binds source, Wasm and CSV.

Pins (`pins.log`): rustc 1.100.0-nightly (`574ff7d98`), cargo 1.100.0-nightly (`7941be6fb`), canbench 0.7.0,
PocketIC server 13.0.0, ic-stable-structures 0.7.2, Lizard 1.24.0; Apple M2 Pro / Darwin arm64 / macOS 26.4.1.
Existing wasm32 release/fat-LTO/codegen-units=1 settings and the normal target directories are reused.
DefaultMemoryImpl remains Ic0StableMemory on wasm and VectorMemory natively, with sixteen distinct
virtual regions. The official stable-memory guidance was checked against the installed 0.7.2 dependency;
no lifecycle or memory API change is part of the experiment.

Every validation command uses `E/run.py` with unique logs, <=300s per command and <=590s cumulative.
Cargo/canbench temporarily use the verified common lock and restore original bytes.
`commands.json` records exact cwd, arguments, exit and elapsed time.

| Check | Result / evidence |
| --- | --- |
| `rustfmt --edition 2024 --config skip_children=true --check crates/ic-stable-lara/src/labeled/graph/traverse/bench/fold.rs` | passed; `format-check.log` |
| `cargo test --locked --offline -p ic-stable-lara --lib --features canbench fold_economics -- --nocapture` | passed: 4 tests, 611 filtered; runtime 1.21s; `native-1.log` |
| `cargo clippy --locked --offline -p ic-stable-lara --lib --tests --features canbench --message-format=json` | exit 0; `clippy-1.log` |
| `python3 E/review.py` | passed; no new-scope warnings/errors, 41 distinct warnings verified against exact HEAD excerpts; `review.log` |
| `canbench bench_t_fold_ --csv --show-canister-output --runtime-path $REPO/crates/ic-stable-lara/.canbench/pocket-ic` from `E/canbench` | passed: 20 exports / 144 scopes, 89.90s including build; `canbench-1.log` |
| `python3 E/analyze.py` | passed: all 36 IC snapshots equal native, exact geometry/capacity and arithmetic checks; `analyze.log` |
| `python3 E/completion_checks.py` | passed: exact scope, protected/source/Wasm/lock/index, whitespace, links and numeric tables; `completion-checks.log` |
| `python3 $AUTHOR_SKILLS/validate_plan.py plans/0369-value-only-fold-economics.md --phase final` | passed; `plan-final.log` |

The four existing `fold_economics_{8,33}_{64,256}` tests now cover three isolated representations:
exact slots/targets/values in both orders, peer preservation, independent fold effects, retained edge
identity/length, released-run reuse and first/eighth append results. Constructor count grows 8→12
across the same four tests, with sequential fixtures within each test. Explicit PocketIC constructor
and installation helpers remain 0→0. The 20 canbench exports reuse the existing runner. No prior
assertion is weakened: both-fold release checks moved to their callers before sharing the probe.

The strict `-D warnings` gate is **not passed**: existing warning debt remains, without suppression.
Full library/workspace/PocketIC suites, independent review, instruction tracing and `--persist` were
not run. Main's value-order and allocator acceptance gates remain separate. No source changed after
the successful native/clippy checks and measured build.

### Complexity and contract review

Lizard 1.24.0, explicit scope on the child only. HEAD has no child file; the saved 0368 source is the
explicit extension baseline, not an invented Git baseline. Named helpers grow 12→16, aggregate
CCN 23→31, function NLOC 375→449, maximum CCN remains 4. Macro-generated wrappers are not expanded.

| Changed helper | CCN before → after | NLOC before → after | Tokens before → after |
| --- | ---: | ---: | ---: |
| `assert_reusable` | 2 → 3 | 27 → 32 | 279 → 294 |
| `workload` | 2 → 3 | 62 → 54 | 306 → 326 |
| `check_fold_economics` | 1 → 1 | 41 → 49 | 294 → 370 |
| `fold_cost` | 2 → 2 | 22 → 23 | 142 → 162 |

The four added helpers own the value-only postcondition (CCN 1), common values scan (1), native append
sequence (2) and value-only timing (2). A single workload still owns all timed read/write order; the
phase string labels observations only. The shared probe adds one necessary retained-plane branch,
without a second memory-ID map or allocator decoder. Existing both-fold preconditions are retained
at `fold_cost`, `check_fold_economics` and the both-fold workload constructor. New callable functions
have at most two inputs excluding the receiver; the new registration macro has four arguments.

Assessment: focused benchmark/test growth. Encapsulation and dependency direction remain unchanged;
existing graph/store APIs own mutation, and the fixture owns observation. No production abstraction,
module, public API, schema, Candid, layout, persistent counter or compatibility path is introduced.
ADR0097's independent-plane contract and the inspected traversal dispatch contracts are unchanged;
0368 receives a follow-up link, not rewritten measurements. A new ADR is not needed for this experiment.

## Next decision

Keep value-only folding as a measured option for property-first read-heavy workloads, with the
hydrated-read trade-off explicit. Do not add a global degree or read-count threshold from these
four states. The0371 shared-owner repair now has failure-boundary and repeated-relocation tests,
plus a rerun of these20 exports. Use that [repaired-source evidence](adr0097-value-span-retirement.md)
for the next policy decision, without treating bounded fixtures as automatic-fold approval.
