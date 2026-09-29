# Selected-slot and hybrid value range reads

Status: implemented and measured; fold/capacity policy unchanged.
Anchor timestamp: 2026-09-29 09:11:03 UTC +0000.

## Result

Contiguous value reads and canonical selected-slot reads replace per-row spill reads and the
`HybridOverflowEdgeReplay` cache. Against the saved [0366 measurements](adr0097-contiguous-spill-traversal.md),
property-first followed by selection uses **60.87% fewer instructions**, incoming hybrid value
batches use 30.53% fewer, and the borrowed-value scan uses 38.14% fewer.

All twelve comparison totals are shown. The baseline was not rebuilt or rerun.

| Benchmark (`bench_t_v_` suffix) | 0366 | 0367 | Instruction change |
| --- | ---: | ---: | ---: |
| `at_replay` → `at_after_values` | 3,986 | 3,455 | −13.32% |
| `bypass` | 4,305 | 3,127 | −27.36% |
| `dense_a` | 12,941 | 13,019 | +0.60% |
| `dense_d` | 12,938 | 13,016 | +0.60% |
| `ebreak` | 8,510 | 8,527 | +0.20% |
| `hyb` | 9,381 | 9,471 | +0.96% |
| `in_dense` | 13,782 | 13,860 | +0.57% |
| `in_hyb_ip` | 111,369 | 77,364 | −30.53% |
| `pf_then_sel` | 266,173 | 104,156 | −60.87% |
| `sparse` | 11,862 | 11,939 | +0.65% |
| `w_ip` | 141,861 | 87,755 | −38.14% |
| `window` | 4,038 | 4,115 | +1.91% |

The topology controls increase by 17–90 instructions after sharing range IO/decode. The shared
checked owner is retained rather than duplicating a separate fast-path implementation; none exceeds
the ADR 0050 5% investigation threshold. These increases are reported, not dismissed as noise.
The old/new selected benchmark names describe the same selected slots after an untimed value scan;
phase one no longer captures a replay. The existing eight-byte fixture retains its codec and shape.

An intermediate `w_ip` result regressed to 193,871 instructions. Inspection found that streaming
property traversal still called the scalar value reader for every row. It now uses bounded value
ranges through the same owner as property-first batches; the final 87,755 result includes that fix.

Every comparison total reports zero heap/stable-memory page increase. This is not a peak-allocation
measurement or evidence of reduced reserved capacity. There is no correctness-equivalent comparison
with main: [0365's main value-order failure](adr0097-current-main-spill-comparison.md) remains open.

## Owning contracts and removed paths

[ADR 0050](../adr/0050-lara-traverse-read-api.md#7-selected-slot-reads-remain-a-first-class-capability)
owns the updated read API.

- `EdgeStore::read_row_range` and its decoder serve full/window topology and adjacent selected
  ranges. Labeled traversal sorts/deduplicates slots, resolves the descriptor once, and dispatches
  tiny/tree readers before slab/run reading. Selected topology never reads gaps between requested
  runs; `Break` prevents later runs from being read.
- `initialized_row_slots` supplies checked prefix/run lengths to dense hybrid admission without
  reading edge bodies. The descriptor plus run header remain the source of truth.
- `ValuesSpillStore::read_range` owns scalar/range output-width and used-range validation. Inline
  cells are bulk-read; widths above eight bytes retain per-site blob-map lookup and exact-width
  validation. The labeled value reader joins independent slab/value-run ranges.
- Selected borrowed-property hydration uses slot-as-ordinal only when initialized extent equals
  live degree. Otherwise it computes live ranks in one ascending topology pass, bounded by the
  largest requested position, and reverses only selected results for descending delivery. It keeps
  original logical slots and reads only selected value runs.
- `HybridOverflowEdgeReplay`, its scratch field, trait associated type/method, capture/tag builder,
  two selected replay helpers, both deferred directional replay methods, and both Graph
  scratch-reusing wrappers are removed. Outgoing and incoming predicate expansion now use ordinary
  selected-slot reads and join their retained phase-one bytes by slot.

Encapsulation and dependency direction stay unchanged: stores own addressing and validation,
labeled traversal owns live-rank/label association, and Graph owns predicate/orientation composition.
No new production type, dependency, persistent summary, or cache replaces the deleted replay.
No layout, Candid, schema, write boundary, fold, promotion, or threshold changes are included.
`LAYOUT_VERSION` remains 1.

Physical slab-span reads remain distinct from live-ordinal slab/run reads. Attempting to merge
those contracts broke the existing promotion fixture
`lpb_in_tree_read_round_trip_at_w_4_stored_4096`: it reads a physical span while its descriptor has
no published value prefix. That delegation was removed, the physical-span contract preserved, and
the unchanged test passes. This is an intentional semantic distinction, not a fallback for old data.

## Contract witnesses

Native library tests increase from 681 to 683. No PocketIC test constructors or canister/federation
install helpers are added (0 → 0); the new cases reuse owning-store fixtures and the existing canbench
runtime.

| Witness | Observable contract |
| --- | --- |
| Existing `bulk_row_four_byte_reads_are_bounded_and_ordered` and ten-byte twin, extended | Slab-only, run-only and split 67-slot rows; both orders; holes; empty, point, adjacent, scattered, full and out-of-range selections; exact slots/targets; exact read calls/bytes; `Break` skips later runs; all six memories preserved |
| New `values_range_reads_exact_widths_and_rejects_slack` | Widths 1/2/8/9/33; full, crossing, tail and empty ranges; exact bytes; slack/output-size rejection; exact missing-blob and width-mismatch errors |
| New `hybrid_range_reads_bound_io_without_topology_prefetch` | Width8/33, 256 logical slots, independent value prefix 32; no phase-one edge-body reads; at most eight value-run reads for width8; full selected topology takes three run reads and `256*4+8` bytes; property memories untouched by topology; all sixteen memories preserved |
| Same hybrid witness after deleting positions 0/64/128 | Selection `[4,1,0]` yields exact live positions 1/4 and their values in both orders; topology rank calculation reads only five rows plus three headers (32 bytes), not the 256-slot extent |
| Renamed owner/phase-two/delete tests | Exact expected identities replace comparisons between two implementations of the removed replay path; same-leaf owners remain separate and in-place tombstones are observed in both orders |
| Existing hybrid, tree-selected, reverse and Graph predicate tests | Hole/live-rank association, width-zero no-property access, width8/33 tree boundaries, exact corruption errors, callback results and incoming/outgoing predicate identities remain covered |

Reading every run for a point selection, scalarizing contiguous reads, prefetching topology in dense
phase one, confusing live ranks with logical slots, or reusing another owner's rows fails these
witnesses. Only obsolete cache assertions were deleted; output and corruption contracts were not
weakened. Snapshots establish unchanged persistent bytes; they do not measure identical-value writes
or transient peak memory.

## New selection/width matrix

`bench_t_range_{v,s}_*` adds fourteen benchmarks, each with three separately timed operations:
`values` is a full descending property-first scan, `selected` is ascending selected topology, and
`hydrated` is descending selected topology with borrowed values. Scopes are **not additive parts of
one predicate query**. Setup and exact slot/target/value assertions run outside timing; borrowed
hydration is checked in both orders.

Every fixture has 256 logical positions and retained edge/value runs. `all8` selects 256 positions,
`point8` selects position 255, `contig8` selects 112..128, and `scattered8` selects every sixteenth
position. Hole cases delete every sixteenth position in 0..255, then append position 255; selection
uses stride 15 and retains sixteen live results. Deletion folds existing values, so the hole cases
have a different value-plane split from the hole-free cases. In particular, the much lower `holes33`
cost is not evidence that holes accelerate blob lookup.

`v` uses VectorMemory. `s` uses `MemoryManager<DefaultMemoryImpl>` with sixteen distinct IDs
0..15; in wasm32, dependency `ic-stable-structures 0.7.2` defines DefaultMemoryImpl as
Ic0StableMemory. This follows the official
[stable-memory skill](https://skills.internetcomputer.org/.well-known/skills/stable-memory/SKILL.md),
checked against the dependency's local `lib.rs` and `memory_manager.rs`. All setup allocations are
outside timing.

| Backend / case | Values | Selected | Hydrated |
| --- | ---: | ---: | ---: |
| stable / all8 | 97,276 | 34,448 | 162,145 |
| stable / point8 | 97,263 | 3,109 | 7,077 |
| stable / contig8 | 97,263 | 5,026 | 22,674 |
| stable / scattered8 | 97,263 | 14,746 | 48,116 |
| stable / holes8 | 124,696 | 16,250 | 55,529 |
| stable / all33 | 3,455,561 | 36,139 | 3,273,223 |
| stable / holes33 | 160,416 | 16,346 | 59,786 |
| vector / all8 | 97,141 | 34,702 | 162,169 |
| vector / point8 | 97,128 | 3,363 | 7,491 |
| vector / contig8 | 97,128 | 5,280 | 23,058 |
| vector / scattered8 | 97,128 | 16,365 | 51,242 |
| vector / holes8 | 125,993 | 18,051 | 56,783 |
| vector / all33 | 3,917,668 | 36,393 | 3,735,256 |
| vector / holes33 | 162,126 | 18,147 | 61,276 |

These are current-only measurements, not speedups over a newly invented baseline. Every total/scope
reports zero heap/stable page increase. Width33's large hole-free cost remains in per-site blob
reads; sparse property hydration still needs prefix liveness counting. Neither is solved by the
edge/value range change.

## Validation and provenance

Worktree: `/Users/yota/dev/gleaph-spill`, branch `spill-swap`, base
`b96ecad6df708a44e182ba7fc98dadd3e2699bf2` plus this uncommitted diff. The main worktree was not edited.
The exact Rust scope is eighteen paths in `rust-paths.json`; documentation scope is ADR 0050 and this
file. Pre-existing dirty paths are separately protected by 267 SHA256 entries in `protected.json`.

Evidence root:
`/Users/yota/.local/share/gleaph/evidence/adr0097/selected-hybrid-range-reads-0367`.
`commands.json` contains exact commands, directories, timeouts, exit codes and elapsed times; logs are
not overwritten. All Rust/build/canbench commands use its bounded `run.py` wrapper, at most 300 seconds
per command and 590 seconds cumulatively. The common lock is borrowed for execution and restored
byte-for-byte afterward. No baseline rebuild, background validation, instruction tracing or persisted
benchmark update was performed.

| Final check | Result / evidence log |
| --- | --- |
| `rustfmt --edition 2024 --config skip_children=true --check` on the exact Rust manifest | passed; `format-check-completion.log` (final prose-only cleanup checked separately) |
| `cargo test --locked --offline -p ic-stable-lara --lib bulk_row` | 2 passed; `bulk-row-1.log`, also covered by final library run |
| `cargo test --locked --offline -p ic-stable-lara --lib` | 683 passed, 0 failed; `lara-lib-completion-2.log` |
| `cargo clippy --locked --offline -p ic-stable-lara -p gleaph-graph --lib -- -D warnings` | passed; `scoped-clippy-completion.log` |
| `cargo check --locked --offline -p gleaph-graph --lib` | passed; `graph-check.log`; final Graph compilation also covered by clippy/tests |
| `cargo test --locked --offline -p gleaph-graph --lib inline_property_predicate` | 11 passed; `graph-predicate-completion.log` |
| `canbench bench_t_v_ --csv --runtime-path /Users/yota/dev/gleaph-spill/crates/ic-stable-lara/.canbench/pocket-ic` | exit 0, 12 totals; `traverse-completion.log/.csv` |
| `canbench bench_t_range_ --csv --runtime-path /Users/yota/dev/gleaph-spill/crates/ic-stable-lara/.canbench/pocket-ic` | exit 0, 14 totals and 42 scopes; `range-completion.log/.csv` |
| Lizard 1.24.0 `--csv -i -1`, baseline/current snapshots of eighteen Rust files | passed; `lizard-completion.log`, `lizard-summary.log`, CSVs and `complexity-summary.json` |

Canbench runs from the external evidence `canbench/` directory. Its build command is
`cd /Users/yota/dev/gleaph-spill/crates/ic-stable-lara && CARGO_TARGET_DIR=../../target/canbench cargo build --locked --offline --release --target wasm32-unknown-unknown --features canbench`.
Saved comparison input: `../contiguous-spill-traversal-0366/traverse.csv` relative to the evidence root.

Intermediate failures remain recorded: the constant-size chunk clippy finding was corrected using
`as_chunks`; the first hole benchmark exposed a fixture edge that discarded its logical slot during
deletion, fixed by the benchmark-only `RangeBenchEdge`; the physical-span delegation failure above
was corrected without changing its test. The final runs supersede these failed candidates; they are
not repeated identical measurements for noise reduction.

Pins:

- rustc/cargo: `1.100.0-nightly (574ff7d98 2026-09-14)`; canbench 0.7.0; PocketIC server 13.0.0.
- Host: Apple M2 Pro, Darwin arm64, macOS 26.4.1 (25E253).
- As-used lock SHA256: `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`.
- PocketIC binary SHA256: `2e38d2266865ae285d612bd533675073d11d0cb77b8365c203431af753ce930e`.
- Measured Wasm SHA256: `bc6f164448eea8ce6b04b5791c01036ef64db640c6be731c6563fa939a33f9df`.

Existing test/benchmark warnings are not suppressed. `GAP-LARA-TEST-CLIPPY`, full-workspace tests,
full PocketIC, unfiltered/persisted canbench and independent review are not run or claimed passed.
The final plan/scope checks are recorded in the completion receipt alongside this evidence.

### Complexity and API review

Working-tree mode against the base above, conventional CCN. Among directly touched production
functions (including added/removed functions), aggregate CCN is **245 → 203**, maximum **51 → 34**,
and NLOC **1,330 → 1,085**. There are 32 matched/added/removed production entries, representing 25
baseline and 19 current functions. Across all measured production functions, CCN is 2,258 → 2,216;
the reduction is not merely branching moved into new helpers. Fixture/test touched CCN is 81 → 123
and NLOC 992 → 1,434; the largest new matrix function has CCN 33. These metrics do not establish speed.

| Production function | CCN | NLOC | Tokens | Decision |
| --- | ---: | ---: | ---: | --- |
| `visit_edges_at_with_inline_property` | 51 → 31 | 241 → 147 | 1,467 → 953 | Keep: one sparse rank pass replaces per-slot mapping and adaptive dispatch |
| `append_ordered_inline_property_bytes_ordinals_next` | 12 → 5 | 61 → 30 | 394 → 202 | Keep: contiguous grouping replaces separate scalar/spanning loops |
| `visit_hybrid_out_inline_property_batches_for_bucket_next` | 14 → 11 | 88 → 67 | 584 → 418 | Keep: checked lengths replace replay capture |
| `visit_edges_with_inline_property_impl` | 32 → 34 | 215 → 226 | 1,081 → 1,196 | Keep: bounded streaming value ranges fix the measured regression |
| `ValuesSpillStore::read_range` | new → 12 | new → 65 | new → 456 | Keep: scalar/range validation and inline/blob decoding have one owner |

New production helpers have at most four meaningful arguments excluding `self`, except
`ValuesSpillStore::read_range`: its five inputs are the externally owned blob map, run, ordinal range,
width and destination, matching the existing scalar reader's contract. No option bag or mode flag is
introduced. Existing visitor signatures are not enlarged; the ordinal appender loses a chain
parameter. `CountMemory` is moved to existing test support, and the slot-preserving edge wrapper is
benchmark-only. Lizard's parser includes receivers and some callback commas in parameter counts;
source signatures, not those raw counts, determine this assessment.

Self-review: retain the owner-level range methods and delete the superseded replay paths. Physical
slab-span semantics intentionally remain separate. No new ADR is required beyond the ADR 0050 API
amendment; persistent formats and write ownership are unchanged. Historical measurement documents
remain reports of their measured revisions, not claims that replay still exists in 0367.

## Remaining decision

No fold threshold is selected. Contiguous inline-value and selected-topology reads no longer need a
fold solely to eliminate scalar run IO. Capacity/reclamation trade-offs, wide blob layout, sparse
rank indexing, and the unresolved 0365 correctness gate require separate decisions and evidence.
