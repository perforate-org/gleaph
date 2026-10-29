# Replaced value spans are reusable at the publication boundary

**Status: Implemented in the spill worktree; scoped validation and batch-fix independent re-review complete.**

The [independent review of 0371](adr0097-value-span-retirement-review.md) approved the
implementation/ownership axis and identified one P2 batch coverage gap. The
[0372 batch witnesses and owning-layer repair](adr0097-batch-value-span-regressions.md) address the
reachable paths and correct the review's admission/reservation assumptions. Final independent
Standards and Spec re-reviews both approve with no findings. The subsequent
[0373 repeated-fold experiment](adr0097-repeated-fold-economics.md) records alternating append/fold/read
totals and reuse separately; it does not choose an automatic-fold threshold. Commit authorization
for 0372 does not authorize committing that later experiment.

The runtime results and numeric tables below remain the pinned 0371 measurements; they are not
claimed as reruns of the 0372 or 0373 source.

The [0370 missing-retirement defect](adr0097-value-prefix-reclamation.md) is repaired at the shared
value-span publication boundary. A fold now returns its old 32-slot prefix to the existing allocator:
**256 bytes at width 8, 1,056 at width 33**. Native tests prove repeated relocation, exact old-offset
reuse without tail growth, and preservation of live values after poisoning the reused source.
Retirement-reservation and destination-allocation failures preserve the pre-existing backing bytes.

The unchanged 20 fold exports now include the repair's cost. Value-only maintenance adds about
15,000 Wasm instructions per fixture (+11.88% at width 8/64, +0.13% at width 33/256). Physical memory
does not shrink. No automatic-fold threshold, orphan-recovery path or layout change is introduced.
The numeric tables in 0368–0370 remain pre-repair observations.

## One graph-owned replacement boundary

The active contract is in [LARA's free-span contract](../storage/lara.md#4-free-span-contract-core-lara).
`LabelBucket` owns the current offset, slot count and width. `EdgeInlinePropertyBytesStore` owns byte
allocation/retirement; its `FreeSpanStore` owns reservation, selection and coalescing. The graph joins
these owners through `publish_bucket_inline_property_bytes_span` in
[`values.rs`](../../crates/ic-stable-lara/src/labeled/graph/values.rs): descriptor first, retirement second.

The call-site audit found and removed duplicate replacement rules in value-slab compaction and four
batch paths: Slab, SlabHoles, RelocatedSlab and ExpandedSlab. Scalar admission, including fold callers,
now uses the same publisher. The batch paths previously retired before descriptor publication;
the common batch preflight now reserves retirement/rollback capacity before allocator mutation.
Materialization-failure cleanup, bucket death, deleted tails and reservation rollback remain separate
operations at their existing owners; they do not replace a live span.

Scalar admission validates the final vertex allocation delta before destination allocation, keeps a
sufficient span even when it is not at the tail, and reserves retirement before relocation. It copies
the old prefix into one zero-padded destination buffer. After allocation, the backed write and
publication are infallible commit work. The unused previous-slot argument and transient
whole-new-span over-accounting/reconciliation are removed. No recoverable error is returned after
this publisher commits; an unexpected post-preflight failure is an invariant violation.

The only production helper added has three inputs besides `self`: bucket slot, previous descriptor
and replacement descriptor. No type, module, mode flag, public API, dependency, schema, MemoryId or
wire format is added. Existing batch commit signatures are not expanded. No new ADR is needed:
ADR0097 §5 links the restored ownership contract and this evidence.

## Regression witnesses

Three tests are added to the existing `values.rs` test module, using three fresh native graphs:

- `value_span_relocation_reclaims_8` and `_33`: force two non-tail relocations (32→64→96 target
  values), separated by distinct live peers. Check exact slot/target/value rows in both orders,
  unchanged edge geometry and run identity/used length, allocation/free/tail balance, exact best-fit
  source reuse, and unchanged rows after overwriting the reused bytes. Finish with in-place growth
  to 97 and sufficient-span admission, neither of which retires live bytes.
- `value_span_relocation_failure_preserves_source`: fill free-record capacity using a bounded,
  allocator-owned scratch reservation, then fail an actual backing grow during retirement preflight.
  Fail destination allocation separately with the existing allocation hook. Both return the expected
  `RebalanceFailed` store error while preserving the vertex, descriptor, accounting and every
  pre-existing byte across all 16 backing regions. Successful retry verifies values and retirement.
  Physical backing reserved by preflight may remain; the test does not require page shrink.

On the unfixed implementation, the two reuse witnesses fail with free bytes 0 instead of 256/1,056;
the failure witness rejects an unexpected successful fold because retirement was never preflighted.
`red-contract.log` records **0 passed / 3 failed, exit 101** at those intended assertions.
`green-contract.log` records **3 passed**. The preliminary `red-tests.log` contains test-code type and
field-access compilation errors; it is not counted as behavioral red evidence.

The final affected run passes **167 tests**: 52 values, 14 batch-write, 20 sibling batch-write tests,
59 compaction, 18 insertion and 4 fold-economics tests. This includes both existing registered-free
value-compaction controls and the batch commit/rollback/failure cases. The nineteenth insertion test,
`g6_tiny_insert_reads_no_edge_bytes`, lives outside the selected `insert::tests` submodule and passes
in a separate exact-filter run. The final evidence therefore covers **168 distinct passing tests**.
No prior assertion is removed or weakened. The four unchanged fold tests still use 12 sequential
MemoryManager fixtures; the three new witnesses use direct VectorMemory/FailpointMemory fixtures.
No new PocketIC constructor or install helper is added.

## Fold costs include reusable ownership

Comparison input is the preserved [0369](adr0097-value-only-fold-economics.md) CSV, not a rebuilt
baseline or main. Benchmark source, fixture dimensions, operation order, common lock, compiler and
runner are unchanged. All 20 exports and 144 scopes complete. Counts are **Wasm instructions**;
maintenance totals include their wrappers and nested scopes are not additive.

| Width / degree | Value-only total: 0369 → repaired | Delta | Both-fold total: 0369 → repaired | Delta |
| --- | ---: | ---: | ---: | ---: |
| 8 / 64 | 126,177 → 141,165 | +14,988 (+11.88%) | 396,825 → 411,639 | +14,814 (+3.73%) |
| 8 / 256 | 576,255 → 591,243 | +14,988 (+2.60%) | 1,072,129 → 1,087,037 | +14,908 (+1.39%) |
| 33 / 64 | 1,292,604 → 1,307,679 | +15,075 (+1.17%) | 1,566,856 → 1,581,839 | +14,983 (+0.96%) |
| 33 / 256 | 11,745,026 → 11,760,101 | +15,075 (+0.13%) | 12,271,700 → 12,286,684 | +14,984 (+0.12%) |

All both-fold edge scopes are instruction-identical to 0369. The value scope carries the additional
work; in the value-only export its increase is 14,908/14,995 instructions, with another 80 in the
wrapper. This is the **net replacement-repair cost**, including changed buffer/accounting work,
not an isolated `retire_byte_span` microbenchmark. The cost increase is accepted for the required
retirement reservation and allocator registration rather than retaining the leak for a cheaper fold.

Unfolded workloads are instruction-identical to 0369. Folded append scopes become cheaper after
removing the admission path's redundant vertex reconciliation: the first append falls by 2,304–2,593
instructions. Folded read-only scopes vary by −582 to +418 instructions (−6.10% to +5.42%). The traversal
source is unchanged, but post-maintenance allocator/heap state and generated Wasm are not identical;
this run does not isolate the source of those small read deltas. They are retained in `comparison.json`,
not presented as a read optimization or assumed to be measurement noise.

Using the repaired-source read counts, the same read-only repayment formula from 0369 gives:

| Width / degree | Values scans: value-only / both | Hydrated scans: value-only / both |
| --- | ---: | ---: |
| 8 / 64 | 23 / 49 | 28 / 19 |
| 8 / 256 | 19 / 26 | 19 / 12 |
| 33 / 64 | 5 / 6 | 5 / 5 |
| 33 / 256 | 4 / 4 | 4 / 4 |

These are arithmetic estimates for unchanged data, not runtime triggers. The charged eight-appends /
one-values-scan episode still fails to repay either fold in all four cases. Its exact scope sums are
retained in `comparison.json`; no sustained or repeated-fold throughput is claimed.

## Reuse, capacity and physical pages remain different measurements

All 36 IC snapshots equal their native counterparts. Compared with 0369, every recorded field is
unchanged except allocator-reported value free bytes in the folded states, including after appends:

| Width / degree | Newly reusable prefix | Folded occupied tail | Folded declared value capacity |
| --- | ---: | ---: | ---: |
| 8 / 64 | 256 B | 776 B | 4,096 B |
| 8 / 256 | 256 B | 2,312 B | 4,096 B |
| 33 / 64 | 1,056 B | 3,201 B | 4,096 B |
| 33 / 256 | 1,056 B | 9,537 B | 9,537 B |

Released spill-run reuse remains covered by the unchanged benchmark assertions. Direct old-slab
reuse is established by the new native tests, not inferred from the free-byte counter alone.
The 16 virtual regions still sum to 32 pages and physical backing stays at 2,049 pages. Every timing
row reports zero heap/stable page increase; this does not measure peak scratch usage or absence of
allocation. No existing orphaned range is discovered or recovered; use fresh experimental state.

## Complexity review

Lizard 1.24.0, explicit scope on `labeled/graph/{values,batch_write,compact,insert}.rs`, conventional
CCN (`--csv -i -1`). All four files were clean at HEAD `24467b17b375b2a7bc462818ef278553a52993fc`;
the final measured snapshots exactly match the tested source. No exclusion or suppression is added.

Ten existing functions are directly touched; one production publisher and four test/helper functions
are added. Across touched functions, aggregate CCN is 239→237 and maximum CCN 52→54. Across all four
files, functions 332→337, CCN 1,784→1,782 and function NLOC 14,116→14,327. Production alone decreases
by 69 NLOC and 18 CCN; the added regression code accounts for 280 NLOC and 16 CCN.

| Function | CCN before → after | NLOC before → after | Tokens before → after |
| --- | ---: | ---: | ---: |
| `ensure_bucket_inline_property_bytes_span` | 33 → 19 | 116 → 86 | 687 → 507 |
| `publish_bucket_inline_property_bytes_span` | new → 3 | new → 18 | new → 86 |
| `compact_inline_property_bytes_slab` | 27 → 26 | 136 → 132 | 853 → 825 |
| `reserve_one_orientation_batch_prepared` | 52 → 54 | 627 → 645 | 2,808 → 2,889 |
| `commit_with_location_mode` | 37 → 33 | 567 → 540 | 2,841 → 2,688 |
| `commit_relocated_slab_run` | 9 → 7 | 218 → 207 | 1,164 → 1,125 |
| `commit_expanded_slab_run` | 13 → 11 | 270 → 263 | 1,492 → 1,454 |
| `check_value_span_relocation` | new → 8 | new → 155 | new → 1,078 |
| `value_span_relocation_failure_preserves_source` | new → 6 | new → 119 | new → 839 |

Assessment: focused ownership consolidation. Keep the common batch preflight's two additional
branches: they reserve required commit/rollback capacity. The existing long batch signatures are
unchanged; no argument bag or general transaction framework is introduced. Admission loses one
argument (four→three besides `self`). Remaining touched functions only lose the obsolete argument or
its local binding. Lizard's function/closure accounting is supporting evidence, not a performance
metric or a reason for unrelated refactoring.

## Reproduction and validation receipts

Evidence root (`E`): `/Users/yota/.local/share/gleaph/evidence/adr0097/value-span-retirement-0371`.
`before.json`, `protected.json`, `before/`, `current/`, `source.patch` and `source.json` separate this
repair from the existing dirty worktree. The benchmark parent/child, allocator source and persisted
benchmark artifacts are protected. The scoped patch comprises four code files and six documents;
the ignored plan is `plans/0371-value-span-retirement.md`.

Pins (`version-pins.log`): rustc 1.100.0-nightly (`574ff7d98`), cargo 1.100.0-nightly (`7941be6fb`),
canbench 0.7.0, PocketIC 13.0.0, Lizard 1.24.0; Apple M2 Pro, Darwin arm64, macOS 26.4.1. Existing
wasm32 release/fat-LTO/codegen-units=1 settings and normal target directories are retained.

- Measured Wasm: 6,550,521 bytes; SHA256 `3b66c0eb683b285315494675d155fb31d687c7d93f5dbc31f30838ba6de58b40`.
- Complete 164-row `canbench/canbench_results.csv` SHA256:
  `de8f6f5d8393b1f75254bfa77d8041bedfb9dfebf6ff4c6bb92c48debf5c9465`.
- Common lock SHA256: `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`.
- Original restored lock SHA256: `15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`.

Commands use `E/run.py`, unique logs, a 300-second per-command cap and 590-second aggregate cap.
Cargo/canbench temporarily use the common lock and restore the original bytes after each command.
`commands.json` records exact commands, cwd, exit and elapsed time. No baseline rebuild, instruction
tracing, background validation or `--persist` is used. Canbench runs from `E/canbench`.

| Check | Result / evidence |
| --- | --- |
| `rustfmt --edition 2024 --config skip_children=true --check` on the four code paths | passed; `format-check.log` |
| `cargo test --locked --offline -p ic-stable-lara --lib --features canbench value_span_relocation -- --nocapture` | intended red 3/3, then green 3/3; `red-contract.log`, `green-contract.log` |
| `cargo test --locked --offline -p ic-stable-lara --lib --features canbench -- --nocapture labeled::graph::values::tests labeled::graph::batch_write labeled::graph::compact::tests labeled::graph::insert::tests fold_economics` | passed 167, filtered 451; runtime 2.58s; `tests-final.log` |
| `cargo test --locked --offline -p ic-stable-lara --lib --features canbench labeled::graph::insert::g6_zero_read_tests::g6_tiny_insert_reads_no_edge_bytes -- --exact` | passed 1, filtered 617; `insert-read-control.log` |
| `cargo clippy --locked --offline -p ic-stable-lara --lib --tests --features canbench --message-format=json` | exit 0; 41 pre-existing Rust warnings, 0 unmatched; `clippy.log`, `lint-review.json` |
| `canbench bench_t_fold_ --csv --show-canister-output --runtime-path /Users/yota/dev/gleaph-spill/crates/ic-stable-lara/.canbench/pocket-ic` | passed 20 exports/144 scopes, 86.52s including build; `canbench.log` |
| Lizard on the explicit before/current snapshots | passed; `lizard-base.log`, `lizard-final.log`, `complexity-summary.json` |
| `python3 E/analyze.py` | passed; 164-row comparison, 36 IC/native snapshot comparisons and complexity; `analyze-final.log` |
| `python3 E/completion_checks.py` | exact scope, protected hashes, source/Wasm/lock/index/HEAD, numeric tables, links and whitespace; `completion-checks.log`, `completion.json` |
| `python3 /Users/yota/.agents/skills/plan/scripts/validate_plan.py plans/0371-value-span-retirement.md --phase final` | final structural receipt: `plan-final.log` |

The first analysis-script invocation used an incorrect export name; it failed before writing results.
`analyze-final.log` uses the actual CSV names. No benchmark rerun was needed for that script fix.
At the validation checkpoint, all 265 protected paths, original lock/index/HEAD and prior numeric
tables were preserved; no commit or main-worktree edit had been made. LARA and ADR0097 are
synchronized; 0368–0370 link this repair without rewriting their measurements.

**Limits:** strict `-D warnings` remains unmet by `GAP-LARA-TEST-CLIPPY`; warnings are not suppressed
or counted as a strict pass. Full library/workspace/PocketIC suites, reopen or upgrade fault injection,
repeated-fold performance policy and persisted artifact updates are not claimed. Independent review
is recorded separately above; it was not part of the original validation checkpoint.
Native failure evidence covers the two stated pre-publication boundaries, not every outer
Graph/Router operation. The previous main value-order and allocator acceptance gates remain separate.
