# Contiguous spill topology traversal

Status: implemented and measured; no fold-policy change.

## Result

Direct slab/spill reads recover the selected topology-scan regressions without folding the spill.
The comparison below uses the saved 0365 spill measurements, not a re-executed baseline. All 15
selected benchmark totals are included; no selected total increases. The one-instruction changes
in the two unchanged hybrid/property-first paths are not attributed to this optimization.

| Benchmark | Spill before | Spill after | Instruction change |
| --- | ---: | ---: | ---: |
| `bench_t_v_at_replay` | 3,986 | 3,986 | 0.00% |
| `bench_t_v_bypass` | 12,250 | 4,305 | −64.86% |
| `bench_t_v_dense_a` | 75,337 | 12,941 | −82.82% |
| `bench_t_v_dense_d` | 71,317 | 12,938 | −81.86% |
| `bench_t_v_ebreak` | 29,913 | 8,510 | −71.55% |
| `bench_t_v_hyb` | 73,674 | 9,381 | −87.27% |
| `bench_t_v_in_dense` | 88,367 | 13,782 | −84.40% |
| `bench_t_v_in_hyb_ip` | 111,370 | 111,369 | ≈0% |
| `bench_t_v_pf_then_sel` | 266,174 | 266,173 | ≈0% |
| `bench_t_v_sparse` | 53,657 | 11,862 | −77.89% |
| `bench_t_v_w_ip` | 154,677 | 141,861 | −8.29% |
| `bench_t_v_window` | 47,504 | 4,038 | −91.50% |
| `bench_l_mix_hub_asc_33x50` | 798,682 | 320,256 | −59.90% |
| `bench_l_mix_hub_ins_33x50` | 59,023,165 | 59,023,165 | 0.00% |
| `bench_l_mix_hub_scn_33x50` | 557,118 | 169,961 | −69.49% |

Heap/stable-memory increases are unchanged for every row. All scans report zero pages of increase;
the mixed insertion control reports 752 heap pages and zero stable pages in both versions. These
VectorMemory-backed controls do not measure stable allocator capacity savings or peak scratch usage.
Insertion code, fold/promotion policy and benchmark fixtures are unchanged.

For context, saved main totals were 18,310/18,056 for dense ascending/descending, 9,819 for the window,
and 452,958/350,316 for mixed ascending/descending. The new topology totals are below those values.
This is not a one-factor log-versus-spill comparison: main has different write/layout policies.
Main's nonzero-value ordering gate in [0365](adr0097-current-main-spill-comparison.md) remains failed;
no correctness-equivalent comparison with main is claimed for property-bearing benchmarks.

## Owning API and execution

[ADR 0050](../adr/0050-lara-traverse-read-api.md#contiguous-slabspill-topology-execution) is the read contract.
`EdgeStore::visit_row_edges(row, order, window, visitor)` uses the existing `CsrVertex` row contract
and the shared `initialized_row_slots` validation. It buffers only the selected initialized slots,
reads slab and spill payloads once each when intersected, and decodes directly in the requested
order. Spill reads retain `SpillRunStore::read_rows` used-range validation. No replay entries,
reversal, persistent summary or second descriptor are introduced.

The labeled adapter decorates logical slots and labels. Full/window topology visitors and
zero-width borrowed/materialized visitors use the same primitive; existing dense nonzero-property
visitors also use it for their topology portion. Tiny/tree mode dispatch remains ahead of these
reads. Symmetric paths inspected and updated: ascending/descending, full/window, bucket/bypass,
borrowed/materialized, and the dense property topology emitter. Duplicate full/window slab loops
and their sparse replay dispatch are removed.

Limits:

- Full scans buffer the initialized row extent, as the former dense slab path did. Windows buffer
  only their selected range. `Break` stops decoding/callbacks, not the already-issued bulk read.
- Nonzero-property hybrid rank mapping, property-first batches, selected-slot replay and core
  streaming iterators retain their existing execution paths. `in_hyb_ip` and `pf_then_sel` remain
  essentially unchanged; this is not a claim that every spill consumer is now optimized.
- No persistent bytes, ordinals, layout versions, public traversal signatures, write ownership,
  compaction, capacity or promotion thresholds changed. No new dependencies or production types.
- New production APIs have at most four meaningful parameters excluding `self`. Existing larger
  property API signatures were not expanded. The shared extent helper returns the two checked
  physical widths; the old prefix accessor is a projection, not a duplicate calculation.

## Contract witnesses

Two new owning-store tests, `bulk_row_four_byte_reads_are_bounded_and_ordered` and
`bulk_row_ten_byte_reads_are_bounded_and_ordered`, cover 67-slot slab-only, spill-only and split rows;
ascending/descending; dense and hole-bearing states; zero, crossing, tail and out-of-range windows;
exact slots/targets; and a distinct `Break` value. Read counters assert both call count and byte
count: at most one payload read per intersecting plane, with spill header reads counted separately.
Reading the whole run for a small window, scalarizing the reads, counting only live rows for the
window, reversing slots incorrectly or exposing allocation slack fails these witnesses.

The same fixtures snapshot all six store memories and check that scans and rejected invalid total
extents make no persistent changes. A real retained run with an undersized core-vertex total must
return exactly `CollectAllocationOverflow` before any callback. Existing spill-store corruption
and out-of-used-range tests remain unchanged and pass in the full native suite.

Existing labeled spill fixtures now additionally check materialized slot/target/label metadata and
window output in both orders, including retained bypass. Existing borrowed-value, Break, tiny/tree,
selected replay and independent value-plane tests remain intact. No assertions were weakened.
Native library test count is 679 → 681; no new PocketIC constructors or installs (0 → 0).

## Validation and provenance

Worktree: `/Users/yota/dev/gleaph-spill`, `spill-swap`, HEAD
`1d3202bc35e098b1220fecf9401503fb83abb19c` plus the scoped working diff and pre-existing dirty files.
All pre-edit Lara Rust sources were hash-matched to 0365. The baseline was not rerun; insertion
code and benchmark fixtures were not edited.

Evidence directory:
`/Users/yota/.local/share/gleaph/evidence/adr0097/contiguous-spill-traversal-0366`.
It contains complete logs, `commands.json` with exact commands/timeouts/monotonic durations,
`comparison.csv/json`, baseline binding, source hashes, the measured Wasm, scope checks and Lizard CSVs.

| Check | Result |
| --- | --- |
| `cargo test --locked --offline -p ic-stable-lara --lib bulk_row` | 2 passed |
| `cargo test --locked --offline -p ic-stable-lara --lib labeled::graph::spill_read_test` | 10 passed |
| `rustfmt --edition 2024 --config skip_children=true --check` on the six changed Rust files | passed |
| `cargo clippy --locked --offline -p ic-stable-lara --lib -- -D warnings` | passed |
| `cargo test --locked --offline -p ic-stable-lara --lib` | 681 passed, 0 failed |
| `canbench bench_t_v_ --csv --runtime-path …/.canbench/pocket-ic` | exit 0; 12 totals |
| `canbench bench_l_mix_hub_ --csv --runtime-path …/.canbench/pocket-ic` | exit 0; 3 totals |

The two canbench calls use an external output/config directory, the normal target directory, and
0365's locked/offline release wasm32 build command with feature `canbench`. No `--persist`, tracing,
full workspace tests or full PocketIC run. Rust/benchmark command time was 96.12 seconds total;
longest command 59.07 seconds. Existing test/benchmark warnings were not suppressed;
`GAP-LARA-TEST-CLIPPY` was not rerun or waived.

Pins:

- rustc: `1.100.0-nightly (574ff7d98 2026-09-14)`; canbench: `0.7.0`.
- As-used common lock SHA256: `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`.
  Borrowed only for checks, then byte-restored; dependency resolution was not changed.
- PocketIC binary SHA256: `2e38d2266865ae285d612bd533675073d11d0cb77b8365c203431af753ce930e`.
- Measured Wasm SHA256: `e468551bb2f9a26b1c6feb634b976262a6750e7d91c326f3d1d0be79b04c1846`.

### Complexity review

Lizard 1.24.0, scoped working-tree comparison with HEAD, conventional CCN, `--csv -i -1`:
six Rust files measured. Directly touched production functions (including new/replaced helpers)
have aggregate CCN **191 → 137**, maximum **76 → 41**, and NLOC **960 → 807**. The row visitor adds
CCN 17 for checked range selection and the two physical planes; the old window visitor falls from
76 to 41. The new test matrix is CCN 20 and reuses each small store fixture across windows.
These are complexity measurements, not instruction-cost attribution. Self-review found no new
flags, redundant persisted facts, unrelated optimizations or enlarged production signatures.
Independent review was not run.

## Next decision

No fold threshold is selected. The measured topology paths no longer require fold for scan speed.
A subsequent decision can therefore evaluate whether capacity/reclamation benefits justify a fold,
using physical capacity and mutation cost rather than the removed replay overhead. The existing
six paged-map ceilings remain 4/6, and 0365's main value-order blocker remains open.
