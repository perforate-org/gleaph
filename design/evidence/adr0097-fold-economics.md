# Fold economics: wide-value reads benefit, physical capacity does not shrink

**Status: Experimental — preserved pre-retirement-repair measurements; no automatic fold policy or threshold change.**

These are the recorded 0368 measurements. [0369 value-only folding](adr0097-value-only-fold-economics.md)
adds the retained-edge-run arm and extends the shared workload to eight appends plus a final values
scan. Its workload totals have a different operation count; the measurements below are preserved.

For the four dense fixtures measured here, folding both planes pays back after **4–6 full property
scans at width33**, compared with **26–47 at width8**, assuming no intervening writes. Width8's next
single append becomes about 36% more expensive. The released spill runs are demonstrably reusable,
but neither virtual nor physical memory page counts fall. These results support investigating
**value-only folding for read-heavy wide values**, not a universal degree/run-length threshold.

This is a current-source, unfolded-versus-folded comparison, not a comparison with main or an earlier
Wasm. [0367](adr0097-selected-hybrid-range-reads.md) already removed repeated scalar IO from ordinary
selected/value traversal. The remaining benefit measured here comes from changing the representation
through existing maintenance, including moving wide values out of their blob-backed run.

## Four identical-data comparisons, with independent storage owners

Each fresh graph has one vertex, a target label with 64 or 256 edges, and one neighboring label with
one edge. The neighbor is inserted after the target's first 32 edges, preventing value-prefix tail
growth. Width is 8 or 33 bytes; target rows are 4 bytes. Values are unique per logical slot; all
fixtures are hole-free and remain below tree promotion.

Construction and the one measured append use `insert_edge_skip_leaf_cascade` with Insertion order,
the existing internal path used by the range fixtures. No maintenance behavior inside that path is
changed. This is not the cost of the ordinary cascading insert API or Graph/Router execution.

| Initial target degree | Edge slab prefix / used run | Value slab slots / used run | Edge / value run class |
| --- | --- | --- | --- |
| 64 | 0 / 64 | 32 / 32 | 64 / 32 |
| 256 | 0 / 256 | 32 / 224 | 256 / 256 |

These are observed descriptors/run headers, not an assumption that a benchmark named dense is
slab-only. Every before/after arm starts with the same logical data and insertion history. The
fixture differs from 0367's range matrix (one neighbor rather than its additional tail neighbor), so
its instruction counts must not be interpreted as a source-performance delta against that matrix.

- Edge maintenance calls `compact_vertex_edge_span_with_moves(src, 0)`, including the existing
  Required span-resolution, relocation, descriptor/accounting updates and run release. This operation
  can touch the peer bucket and is not merely a memcpy of one run. It leaves the values run intact.
- Value maintenance calls `fold_bucket_values_run_into_slab` after edge maintenance. It reads values,
  admits a slab span, writes the values, drops run blob sites and releases the values run. No new
  fold implementation, publication path, cache or storage-statistics API is added.
- `before` and `after` workload exports measure ascending full topology, descending full property
  batches, descending hydrated traversal, and then one append, each in its own named scope. They are
  separate operations, **not components of one query**. Property-batch scratch is warmed outside timing.
- Setup, capacity logging, exact-result assertions and allocator reuse probes are outside `bench_fn`.
  All outputs are validated in both orders, including the neighbor, before read timing and after
  mutation timing. The native witnesses additionally validate the intermediate edge-only state.

`MemoryManager<DefaultMemoryImpl>` uses sixteen distinct MemoryIds, 0–15. On wasm32,
`ic-stable-structures 0.7.2` defines DefaultMemoryImpl as Ic0StableMemory; natively it is VectorMemory.
The backing memory handle is retained separately from virtual regions. This follows the official
[stable-memory guidance](https://skills.internetcomputer.org/.well-known/skills/stable-memory/SKILL.md),
checked against the installed dependency's `lib.rs` and `memory_manager.rs`.

## Fold cost and read-only break-even

All counts are **Wasm instructions**, not elapsed time, cycles prices or asymptotic complexity.
The complete CSV has 12 totals and 100 scopes. The fold totals below include both maintenance
operations and the measurement wrapper; their nested production scopes are not additive.

| Width / degree | Edge fold scope | Value fold scope | Both-fold total |
| --- | ---: | ---: | ---: |
| 8 / 64 | 272,522 | 121,439 | 396,825 |
| 8 / 256 | 497,504 | 571,761 | 1,072,129 |
| 33 / 64 | 275,469 | 1,288,523 | 1,566,856 |
| 33 / 256 | 508,899 | 11,759,937 | 12,271,700 |

For a fixed read operation, the estimate is:

```text
ceil(both_fold_total / (unfolded_read - folded_read))
```

It applies only when the denominator is positive, the folded representation persists, and the same
full scan repeats without writes. It does not amortize the measured append or predict an evolving
read/write workload. The comparison changes both planes; **it does not isolate value-only folding**.

| Width / degree | Full values before → after | Change | Scans to repay both folds |
| --- | ---: | ---: | ---: |
| 8 / 64 | 24,325 → 15,820 | −34.96% | 47 |
| 8 / 256 | 97,415 → 55,555 | −42.97% | 26 |
| 33 / 64 | 326,201 → 20,713 | −93.65% | 6 |
| 33 / 256 | 3,370,304 → 75,135 | −97.77% | 4 |

| Width / degree | Hydrated scan before → after | Change | Scans to repay both folds |
| --- | ---: | ---: | ---: |
| 8 / 64 | 31,006 → 9,541 | −69.23% | 19 |
| 8 / 256 | 119,110 → 27,589 | −76.84% | 12 |
| 33 / 64 | 341,602 → 13,414 | −96.07% | 5 |
| 33 / 256 | 3,449,750 → 42,399 | −98.77% | 4 |

Topology saves only 145–437 instructions per scan in these fixtures. Charging the complete
both-fold operation to topology alone takes thousands of scans (and 43,985 in width33/256). Those
figures are not the break-even for edge-only folding, which was not independently timed as a read arm.

| Width / degree | Topology before → after | One append before → after | Append change |
| --- | ---: | ---: | ---: |
| 8 / 64 | 5,510 → 5,365 | 12,826 → 17,440 | +35.97% |
| 8 / 256 | 13,953 → 13,605 | 12,769 → 17,440 | +36.58% |
| 33 / 64 | 6,263 → 5,826 | 42,281 → 17,532 | −58.53% |
| 33 / 256 | 15,390 → 15,111 | 47,311 → 18,425 | −61.06% |

The single append is state-specific: the unfolded edge run is at a class boundary, while the folded
arm creates a new edge run. Unfolded values have 32 or 224 used rows; folded values can extend the
new slab tail. This does not establish sustained insertion throughput or a repeated-fold policy.
Native checks confirm that the folded arm's append immediately reintroduces an edge run with one row.

For completeness, all eight workload totals are retained below, but they are not used as a query
metric or break-even denominator because they combine three different reads and a mutation.

| Width / degree | Before workload total | After workload total |
| --- | ---: | ---: |
| 8 / 64 | 77,470 | 52,014 |
| 8 / 256 | 246,917 | 118,037 |
| 33 / 64 | 720,017 | 61,155 |
| 33 / 256 | 6,886,425 | 154,740 |

## Reusable runs are not reduced physical memory

In every before/after snapshot, the sixteen virtual regions have **32 pages in total** (2 MiB).
Their common page vector, in constructor/MemoryId order, is:

```text
[1, 7, 1, 3, 1, 5, 1, 1, 1, 3, 1, 1, 3, 1, 1, 1]
```

The physical backing remains **2,049 pages** (128.0625 MiB), because the standard MemoryManager
allocates 128-page buckets for each of sixteen regions plus its management page. The blob region
remains one virtual page. All 12 totals and 100 scopes report zero heap/stable page increase;
this does not measure peak scratch allocation, absence of allocations, or physical shrink.

| Initial degree | Edge run released | Value run released | Total reusable run bytes |
| --- | ---: | ---: | ---: |
| 64 | 260 B | 264 B | 524 B |
| 256 | 1,028 B | 2,056 B | 3,084 B |

Run bytes include the header row and capacity class, using `SpillRunStore::class_rows` and the
existing edge/cell byte widths. An untimed probe allocates the same class through SpillRunStore,
asserts the exact old run ID is returned with unchanged arena high-water and page counts, and
releases it again. It runs natively and in the four IC fold-cost exports. This proves **same-class
reuse**, not that the next one-row append consumes a larger freed class. Blob-map free-node/payload
bytes are not exposed by the inspected owner and are not included in this byte count.

Moving live rows to the slab also consumes capacity:

| Width / degree | Value slab occupied tail before → after | Declared value slab capacity before → after |
| --- | ---: | ---: |
| 8 / 64 | 264 → 776 B | 4,096 → 4,096 B |
| 8 / 256 | 264 → 2,312 B | 4,096 → 4,096 B |
| 33 / 64 | 1,089 → 3,201 B | 4,096 → 4,096 B |
| 33 / 256 | 1,089 → 9,537 B | 4,096 → 9,537 B |

The target's value span grows from 32 to N slots. With the peer blocking in-place growth,
`ensure_bucket_inline_property_bytes_span` allocates a new N×W span; its relocation branch does not
retire the old span to the free-span owner. Recorded value free bytes remain zero. Thus the old
32×W prefix must not be counted as reclaimed. [0370 reclamation diagnosis](adr0097-value-prefix-reclamation.md)
confirms that explicit later compaction also leaves it unreusable at degree64, widths8/33.
[0371 repairs retirement and remeasures the unchanged fold workload](adr0097-value-span-retirement.md).
The zero-free-byte observation and instruction counts here describe the pre-repair source.

The vertex's edge span grows from 1 to N+1 slots. Declared edge-slab capacity grows from 4,112 to
4,208 slots at N=64, or 4,416 at N=256; allocator-reported free edge slots grow from 0 to 16.
Those are slots (4 bytes each), despite that statistics field's `free_bytes` name. These changes
all fit within existing pages. Reusable runs, slab free spans, allocated high-water and physical
pages are distinct measurements; summing their deltas into a claimed memory saving would be wrong.

The eight IC before/after snapshots exactly match the corresponding native snapshots. Native logs
also retain edge-only and post-append states; JSON `degree` is the initial fixture size in all phases.

## Contract checks and change surface

Four new tests (`fold_economics_{8,33}_{64,256}`) protect exact slot/target/value association, both
orders, unchanged neighbors, no movement notifications for dense folding, independent edge/value
fold effects, append correctness on each arm, and same-class run reuse. They extend coverage without
changing or weakening any existing contract. Skipping folds fails run-state/reuse assertions;
misordering or dropping rows fails exact sequences, not merely counts.

Each native test creates two sequential graph fixtures: one for the unfolded append and one for the
fold/reuse/append sequence (8 graph/MemoryManager initializations across four tests). Explicit
PocketIC constructors and canister/federation install helpers remain 0 → 0; the twelve canbench
exports use the existing runner. Full lifecycle, fault injection, holes, tree modes and Graph/Router
execution remain outside this measurement.

The only existing Rust edit is `mod fold;` in
`crates/ic-stable-lara/src/labeled/graph/traverse/bench.rs`. The child
`bench/fold.rs` reuses its parent's codec and adds only experiment code behind the existing canbench
feature. `FoldFixture` owns fixture state and memory observations, not a new production abstraction.
Production code, public APIs, Candid, schema, layout, fold/promotion triggers and write boundaries are
unchanged. No new dependency, persistent counter, decoder or compatibility branch is introduced.

Design review checked ADR0097's independent planes, movement publication and retained-run append,
plus ADR0050's traversal contracts. These contracts remain unchanged; this new evidence document is
the only design change. The existing [0365 main value-order gate](adr0097-current-main-spill-comparison.md),
allocator acceptance gates, and `GAP-LARA-TEST-CLIPPY` are not closed by this experiment.

### Complexity review

Lizard 1.24.0, working-tree mode against `24467b17b`, measures exactly the parent and new child.
Parent function bodies are unchanged. The child has no invented baseline: Lizard reports 12 named
helpers, aggregate CCN 23, maximum CCN 4 and function NLOC 375; macro-generated wrappers are not
expanded. All growth is benchmark/test code, with zero production changes.

| New helper | CCN | NLOC | Assessment |
| --- | ---: | ---: | --- |
| `FoldFixture::new` | 3 | 53 | Fixed four-case construction and independent backing handle |
| `FoldFixture::assert_rows` | 4 | 72 | Exact three-API, two-order oracle shared by tests and benchmarks |
| `FoldFixture::snapshot` | 2 | 38 | Untimed owner-derived capacity, no duplicated wire decoder |
| `FoldFixture::assert_reusable` | 2 | 27 | Exact allocator identity/high-water witness |
| `workload` | 2 | 62 | Four explicitly separated timing scopes with append last |
| `check_fold_economics` | 1 | 41 | Independent-plane and post-append native witness |

All functions have at most two meaningful inputs excluding the receiver. The six macro arguments
are four export/test identifiers and the two fixture dimensions, not an expanded production API.
Keep the bounded experiment; no generalized benchmark framework or production extraction is needed.
The constructor repeats a small amount of MemoryManager setup because it must retain the physical
handle that the parent range helper does not return. No obsolete production path is retained.

## Reproduction and validation

Evidence root:
`$GLEAPH_EVIDENCE/adr0097/fold-economics-0368` (`E` below).
Base: `24467b17b375b2a7bc462818ef278553a52993fc`, plus this benchmark-only source patch and the protected
pre-existing worktree state. `before.json`, `protected.json`, source snapshots, `measured.json` and
`completion.json` bind those states separately. Main is not a comparison input and was not edited.

Toolchain: rustc 1.100.0-nightly (`574ff7d98`), cargo 1.100.0-nightly (`7941be6fb`), canbench 0.7.0,
PocketIC server 13.0.0, ic-stable-structures 0.7.2. Host: Apple M2 Pro, Darwin arm64, macOS 26.4.1.
Existing wasm release/fat-LTO/codegen-units=1 settings and normal `target/canbench` are retained.

- Measured Wasm: 6,577,892 bytes; SHA256 `d06b4791bb952840d7647984412f0de84cf65214cbeb509e310d6ca562f81304`.
- As-used common lock: `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`.
- Original lock, restored after every cargo/canbench command: `15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`.
- `fold-results.csv`: all 112 rows; SHA256 `1e43dceba56d406f5791cd28e204ecf510adfd602773b9acacb1493bc7e99b2f`.
- `comparison.json` has exact deltas and break-even arithmetic; `capacity.json` has native/IC snapshots.

All Rust/IC execution uses `E/run.py`, with unique logs, a 300-second per-command cap and a
590-second cumulative cap. `commands.json` records exact commands, cwd, exit codes and elapsed times.
The external canbench config builds locked/offline wasm in the existing target directory. No old
baseline rebuild, instruction tracing, background validation or persisted benchmark update is used.

| Check | Result |
| --- | --- |
| `rustfmt --edition 2024 --config skip_children=true --check` on the two Rust paths | passed; `format-final.log` |
| `cargo test --locked --offline -p ic-stable-lara --lib --features canbench fold_economics -- --nocapture` | passed: 4 tests, 611 filtered; `native-final.log` |
| `cargo clippy --locked --offline -p ic-stable-lara --lib --tests --features canbench --message-format=json` | exit 0; no new-scope warnings/errors; `clippy-final.log` |
| `python3 E/review.py` | passed: 41 distinct existing Rust warnings verified against exact HEAD excerpts; complexity/source/protected checks; `review.log` |
| `canbench bench_t_fold_ --csv --show-canister-output --runtime-path $REPO/crates/ic-stable-lara/.canbench/pocket-ic` from `E/canbench` | passed: 12 exports, 100 scopes, all output/reuse assertions; `canbench-1.log`, about 81s including build |
| Exact source/protected/lock/index/whitespace and document numeric checks | passed; `completion-checks.log` |
| `python3 $AUTHOR_SKILLS/validate_plan.py plans/0368-fold-economics.md --phase final` | passed; `plan-final.log` |

The initial native compile rejected two incorrect diagnostic accessor assumptions; only the new
snapshot code was corrected. An initial clippy warning in the new oracle was corrected to use
`as_chunks`. Final native/clippy checks and the measured Wasm use the corrected source. The strict
`-D warnings` test/canbench gate remains unmet by the existing warning debt; it was not suppressed
or reported as passed. Full library/workspace tests, full PocketIC, independent review and
`--persist` were not run for this benchmark-only slice.

## Next decision

Do not set a global fold threshold from four fixed states. The **value-only folding with retained
edge runs** comparison and bounded read/write episode are now measured in
[0369](adr0097-value-only-fold-economics.md). That follow-up quantifies the maintenance savings and
remaining read-path trade-off without changing production policy or format. Old value-prefix
retirement is repaired and remeasured in [0371](adr0097-value-span-retirement.md); that bounded repair
does not establish a repeated-fold capacity policy.
