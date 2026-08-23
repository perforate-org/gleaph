# Tombstone-heavy OFFSET skip research

Date: 2026-08-23 02:52:08 UTC +0000
Last audited: 2026-08-23 03:44:51 UTC +0000
Status: bounded evidence complete; **ADR not justified**
Plan: 0283-tombstone-offset-skip-research

## Decision

Tombstone-heavy live-row OFFSET scans are materially more expensive than dense or offset-zero
queries. Benchmark-only fixed-block and two-level selectors both cleared the predeclared query
threshold when their complete measured paths included candidate selection, canonical stable-row
decoding, liveness checks, slot reconstruction, and identical visitor work.

That result does not justify persistent metadata. The reproducible audit artifact is now the
primary decision evidence. Existing `DeferredLabeledLaraGraph` maintenance restored the matched
query to 27,095 instructions, which is lower than the winning two-level candidate's 59,210
instructions. The sign-corrected optimistic query-only upper-bound crossover is:

`S_candidate_over_post = I_candidate_e2e - I_post_query = 59,210 - 27,095 = 32,115`;

`Q_upper = I_maintenance_total / S_candidate_over_post = 831,216,051 / 32,115 = 25,882.49
queries`.

This is only an optimistic upper bound: it omits candidate build, mutation, update, validation,
framing, and repair costs, while compaction retains independent storage obligations. A positive
query-only crossover alone does not establish substitution or workload benefit. The exact sparse
scan and current compaction owner remain authoritative. No production layout, mutation path,
public API, Candid surface, migration, or ADR changed.

## Fixture and measured-query contract

The retained benchmark family builds one overflow-free labeled slab with 1,024 live rows at logical
extents 1,024 / 2,048 / 4,096 / 8,192 and survivor strides 1 / 2 / 4 / 8. This gives exact 0% / 50%
/ 75% / 87.5% tombstone densities while keeping survivor count fixed. Setup, compaction, deletion,
and assertions remain outside `bench_fn`.

Each fixture asserts the bucket degree, stored extent, tombstone count, overflow-free shape, exact
survivor slots and targets, ascending/descending order, zero-limit behavior, OFFSET 960 / LIMIT 32,
and boundary offsets 31 / 32 / 33. Immediately before every retained measured query closure, the
exact measured window is collected and compared as an ordered `(slot, target)` sequence against
the request-ordered truth slice for that density, order, and offset. It must also return
`ControlFlow::Continue`, exactly 32 rows, and no tombstone. This covers all ten retained cases,
including every offset-zero control; count/checksum equality alone is not accepted as preflight
proof.

The measured visitor performs only fixed count, slot, target, checksum, and `black_box` work. It
does not allocate a result vector.

## Canonical query evidence

Primary audit command: `canbench --csv --hide-results offset_live1024` from
`crates/ic-stable-lara`, after removing the temporary prototypes. The exact CSV and command are in
[`artifacts/0283-tombstone-offset-canbench.txt`](artifacts/0283-tombstone-offset-canbench.txt).
Heap and stable-memory deltas were zero.

| Density | Extent | Order | Offset | Instructions |
| --- | ---: | --- | ---: | ---: |
| 0% | 1,024 | ascending | 0 | 27,146 |
| 0% | 1,024 | ascending | 960 | 27,146 |
| 0% | 1,024 | descending | 0 | 27,067 |
| 50% | 2,048 | ascending | 0 | 17,339 |
| 50% | 2,048 | ascending | 960 | 235,398 |
| 75% | 4,096 | ascending | 0 | 24,243 |
| 75% | 4,096 | ascending | 960 | 447,810 |
| 87.5% | 8,192 | ascending | 0 | 38,051 |
| 87.5% | 8,192 | ascending | 960 | 872,634 |
| 87.5% | 8,192 | descending | 0 | 37,343 |

| Density | OFFSET / same-density offset-zero | OFFSET / dense OFFSET |
| --- | ---: | ---: |
| 50% | 13.5762x | 8.6716x |
| 75% | 18.4717x | 16.4964x |
| 87.5% | 22.9333x | 32.1460x |

The current-cost materiality gate passes at every tombstone density. The original research build
remains corroborating historical evidence, not the decision source: its omitted 50% ratios were
`233,553 / 15,494 = 15.0738x` and `233,553 / 27,146 = 8.6036x`; its omitted 75% ratios were
`445,972 / 22,405 = 19.9050x` and `445,972 / 27,146 = 16.4286x`. Its 87.5% ratios were 24.00x and
32.08x. The verdict below depends on the reproducible primary artifact instead.

## Candidate evidence

The fair candidate reader was benchmark-only and resolved the vertex, single label bucket, and slab
read context once per query. It then read each selected physical row from the canonical stable edge
store, applied slot and label reconstruction, rejected deleted/tombstoned rows, decoded the edge,
and performed the same 32-row visitor checksum as production. It did not use the unfair per-slot
`Traversal::read_edge_state` path, precompute answer rows, or add a production/test-only API.

| Candidate | 75% instructions | Ratio to 75% current | 87.5% instructions | Ratio to 87.5% current |
| --- | ---: | ---: | ---: | ---: |
| Fixed 32-slot counts | 35,620 | 7.9543% | 65,776 | 7.5376% |
| Two-level 32x32 counts | 32,138 | 7.1767% | 59,210 | 6.7852% |

The exact primary prototype trigger used the 8,192-slot fixed-block selector-only probe:

- `N_fixed_block_directory_lookup_8192 = 6,468` instructions;
- `D_fixed_block_selector_probe_8192 = 7,463` instructions;
- `F_two_level_8192 = 6,468 / 7,463 = 0.8667`.

Because 0.8667 is at least 0.25, the two-level prototype was required and measured. It was the
winning query-only candidate. Bitvector rank/select was not prototyped because a block-count
candidate already cleared the query gate. All fixed-block, two-level, selector-only, and maintenance
prototype code and benchmark exports were removed after capturing evidence.

The exact temporary source is retained as
[`artifacts/0283-tombstone-offset-prototype.patch`](artifacts/0283-tombstone-offset-prototype.patch).
It proves that both candidates ran the canonical one-bucket stable read/decode/liveness/checksum
path. Before each measured closure, fixed-block and two-level selection were checked for every one
of the 1,024 live ordinals in both orders, the explicit 31 / 32 / 33 / 959 / 960 / 991 / 1,023
boundaries, one disposable truth-copy row-toggle-plus-directory-rebuild accounting check, and exact
32-row window parity. The accounting check decremented the owning block/group counts without
claiming a canonical graph mutation test. The
original research-build totals (37,568 / 69,584 fixed and 34,148 / 63,080 two-level, trigger
6,464 / 7,459 = 0.8666) are historical corroboration only.

Analytical metadata before framing/allocator overhead remains:

| Shape | Bytes at extent 8,192 | Bytes/slot | Update touches | Rebuild | Exact select bound |
| --- | ---: | ---: | --- | --- | --- |
| Fixed 32-slot `u16` counts | 512 | 0.0625 | one counter | O(extent) | at most 256 counters |
| Two-level 32x32 `u16` counts | 528 | 0.064453125 | two counters | O(extent) | at most 8 groups + 32 blocks |
| Bitvector plus 512-slot counts | 1,056 | 0.12890625 | one bit plus one count | O(extent) | at most 16 groups + 8 `u64` words + in-word select |

Both count candidates are below 0.25 logical metadata bytes per physical slot and have O(1)
analytical update touches, but those analytical properties are not production mutation evidence.

## Existing maintenance comparator

The comparator used a fresh `DeferredLabeledLaraGraph`. Deletions went through
`remove_edge_matching`, which admitted the real tombstone-pressure maintenance item.

The primary pre-drain comparator was freshly reproduced at `2026-08-23 03:44:51 UTC +0000` with
`canbench --csv --hide-results offset_live1024_audit_maintenance_pre_query_875` from
`crates/ic-stable-lara`. Its exact raw row is retained in the canbench artifact; it measured
870,788 instructions with zero heap and stable-memory increase before any maintenance call.

Every drain call used exactly:

`MaintenanceBudget { max_instructions: 5_000_000, reserve_instructions: 0,
checkpoint_every: 1, max_work_items: Some(1), max_segments: Some(1),
max_delete_edge_steps: Some(1) }`.

| Measurement | Instructions | Calls/work items | Rebalanced segments |
| --- | ---: | ---: | ---: |
| Pre-drain query | 870,788 | 1 query | n/a |
| Complete drain, checkpoint 1 (`I_maintenance_total`) | 831,216,051 | 1,025 | 1 |
| Maintenance-item scope | 824,414,511 | 1,025 | 1 |
| Matched drain, checkpoint max | 830,986,451 | 1,025 | 1 |
| Post-drain query | 27,095 | 1 query | n/a |

Every bounded call was asserted to process exactly one work item, report the actual remaining queue
length, and make progress; the queue reached zero. canbench aggregates the 1,025 maintenance-item
scope measurements rather than emitting an individual instruction row for each call. The production
comparator numerator is explicitly `I_maintenance_total = 831,216,051`; the maintenance-item scope
is 824,414,511 and the measured outer overhead is 6,801,540. The matched checkpoint difference is
229,600 instructions total, about 224 instructions per call. Post-drain assertions proved
`stored_slots == degree == 1,024`, packed logical slots, and preserved target order. The original
research-build values (831,349,272 total, 824,541,591 item scope, 6,807,681 outer overhead, and
831,119,672 checkpoint-max) are historical corroboration only.

## Retention and design outcome

The canonical density matrix is retained as permanent benchmark evidence because it isolates live
OFFSET scaling across tombstone densities and includes exact pre-measure correctness guards. After
that positive retention decision, unfiltered `canbench --persist` completed all 151 benchmarks and
added exactly the ten canonical matrix entries. The complete artifact diff is 70 added lines; three
unrelated suite remeasurements were restored to their prior values, so no pre-existing result and
no candidate or maintenance name remains changed.

The audit did not rerun unfiltered persistence. The focused retained family completed with all ten
benchmarks classified unchanged; its small -8-instruction non-dense deltas remain below canbench's
significance threshold and were not written to `canbench_results.yml`.

`GAP-2026-07-25-002` is corrected only in its Status, Evidence, and Next decision fields, with the
ledger's date-only `Last updated` and full UTC `Anchor timestamp` synchronized to the same
2026-08-23 research event. The prior stale ledger wording was accidentally committed with unrelated
Plan 0282 work in `b787ac389`; this slice does not rewrite history. ADR 0050, ADR 0048, storage
layout, and MemoryId inventory remain unchanged because the verdict is no ADR.

## Unresolved production obligations

Any future persistent skip design must still establish insert/delete atomicity, ordered/unordered
tombstone reuse, overflow folding, compaction relocation, forward/reverse co-updates, reopen
validation, corruption rejection, rebuild/repair, fragmentation, stable framing/MemoryId ownership,
and integrated mutation/query benchmark cost. The benchmark-only candidate proves none of these.

## Validation record

Completed research and retained-diff runs:

- `cargo check -p ic-stable-lara --lib --features canbench`;
- `canbench --show-summary offset_live1024`;
- `canbench --show-summary offset_live1024_candidate_fixed_block`;
- `canbench --show-summary offset_live1024_candidate_two_level`;
- `canbench --show-summary offset_live1024_maintenance_pre_query_875`;
- `canbench --show-summary offset_live1024_maintenance_drain_checkpoint1_875`;
- `canbench --show-summary offset_live1024_maintenance_drain_checkpoint_max_875`;
- `canbench --show-summary offset_live1024_maintenance_post_query_875`;
- `canbench --csv --hide-results offset_live1024`;
- `canbench --persist` (unfiltered, 151 benchmarks, 10 new permanent entries);
- `cargo test -p ic-stable-lara --lib visit_edges_window` (2 passed, 0 failed, 502 filtered out);
- `cargo clippy -p ic-stable-lara --lib --features canbench -- -D warnings`;
- `canbench --show-summary offset_live1024` (10 unchanged against the persisted artifact);
- `git diff --check -- crates/ic-stable-lara/src/labeled/graph/traverse/bench.rs
  crates/ic-stable-lara/canbench_results.yml design/implementation-gaps.md`;
- `git diff --no-index --check /dev/null
  design/investigations/2026-08-23-tombstone-offset-skip-research.md` (exit 1 because the file is
  new; no whitespace diagnostics);
- `python3 /Users/yota/.agents/skills/plan/scripts/validate_plan.py
  plans/0283-tombstone-offset-skip-research.md --phase final`.

Completed final-audit reproduction:

- `canbench --csv --hide-results offset_live1024_audit_maintenance_pre_query_875` (1 temporary
  Deferred pre-drain query benchmark; 870,788 instructions; heap/stable-memory delta 0; exit 0;
  exact UTC/raw row retained in the audit artifact);
- `canbench --csv --hide-results offset_live1024_audit` (8 temporary audit benchmarks; exact CSV
  retained in the audit artifact);
- `canbench --csv --hide-results offset_live1024` (10 unchanged; no persistence);
- `rustfmt --edition 2024 --check crates/ic-stable-lara/src/labeled/graph/traverse/bench.rs`;
- `cargo check -p ic-stable-lara --lib --features canbench`;
- `cargo test -p ic-stable-lara --lib visit_edges_window` (2 passed, 0 failed, 502 filtered out);
- `cargo clippy -p ic-stable-lara --lib --features canbench -- -D warnings`;
- `git apply --check
  design/investigations/artifacts/0283-tombstone-offset-prototype.patch`;
- `python3 /Users/yota/.agents/skills/plan/scripts/validate_plan.py
  plans/0283-tombstone-offset-skip-research.md --phase final`;
- exact prototype patch capture against the retained post-preflight benchmark source;
- removal of every temporary helper, import, export, benchmark name, and generated CSV from the
  compiled/source tree after artifact capture.

Plan 0283's terminal document, scope-manifest, patch-application, absence, and final-validator
evidence is retained in
[`artifacts/0283-final-validation.txt`](artifacts/0283-final-validation.txt).

`cargo fmt --all -- --check` was rerun but is not a workspace pass: it reports only unrelated dirty
formatting in the `gpui-graph` paint benchmark, Graph canonical export/catalog/query files,
PocketIC targets, and Router benchmark/schema-migration files. The affected `ic-stable-lara`
benchmark file passes the focused `rustfmt --check`. No PocketIC runtime, Graph/Router test,
full-workspace test, unfiltered canbench persistence, or Criterion run was required because this
slice changes no production or canister-boundary behavior and preserves the existing +70-line YAML
artifact diff.
