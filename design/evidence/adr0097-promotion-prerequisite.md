# ADR 0097 promotion prerequisite

Implementation: `15ab44efb`, on `spill-swap`. This is a bucket-promotion correction, not public capacity integration or main-landing approval.

## Contract and ownership

`promote_bucket_to_tree_mode_impl` no longer compacts edge runs or folds values before admission. It uses initialized edge prefix plus used spill rows, including holes. The 4-byte fixture retains prefix 5 + run 1024 = 1029 slots with 0, 8 and 16 deletions. `T_PROMOTE` stays 1024. Wide edges still receive the exact unsupported-width error.

The source visitor supplies surviving slot identities and resolves values through their independent live-order prefix/run mapping. Unpublished native edge/property blocks retain those identities; holes become native tombstones with exact leaf-header counts. The descriptor publishes the original high-water extent and live degree. Only afterward are old runs and eligible spans retired. Tree visitors intentionally expose tombstone rows: tests assert the complete native stream in both directions and its header metadata, rather than hiding tombstones with a filter.

One interior-block builder serves both target streams. Existing property sizing remains the source of truth. Counts-row preflight previously duplicated in bypass conversion now shares `EdgeStore::counts_leaf_index` with the actual count update and promotion. No runtime type, layout, threshold, compaction policy, or benchmark fixture was added or changed.

## Runtime evidence

Artifacts and reproduction sources: `/tmp/promotion-prereq/`.

- `final-tests.rs` is the committed seven-test fixture. `final-red.log` runs it against `promote-before.rs`: **1 passed / 6 failed**, exit 101. The old promoter mutates rejection states, renumbers or rejects hole-bearing runs, and fails allocation cleanup assertions.
- Final library: `timeout 120 cargo test -p ic-stable-lara --lib`, **644 passed / 0 failed**, exit 0 (`library-final.log`). This includes all seven promotion cases.
- Sidecars: widths 8 and 33, distinct values, a second label preventing tail-only value growth, independently sized source prefixes/runs, with and without holes. Both directions and native tombstone headers are checked.
- Allocation failure: after two unpublished blocks are minted (property and edge), the next LTB page grow fails. Exact result is `Store(RebalanceFailed(GrowFailed { current_size: 1, delta: 1 }))`. Both blocks return to the free list; allocator accounting proves the complete root span returned. Source bucket, vertex, rows/values, and global count remain unchanged. Clearing the fault allows promotion. Pages are 64 KiB.
- `timeout 120 cargo clippy -p ic-stable-lara --lib -- -D warnings`: passed (`clippy-final.log`). Initial redundant casts were corrected; initial diagnostics remain in `clippy.log`.
- Scoped `rustfmt --edition 2024 --config skip_children=true --check` for the five changed Rust files: passed (`fmt-final.log`). `git diff --check`: passed.
- From `crates/ic-stable-lara`: `timeout 210 canbench bounded_inline_promote_full --csv`, exit 0, **444,548 instructions**, unchanged versus the preserved full artifact. `focused.csv` contains the exact row; `bench-promote.csv` contains console output and `bench-promote.log` the build transcript. This measures that benchmark's traversal, not promotion allocation cost. No `--persist` was used. The generated focused workspace CSV was copied out and the original complete CSV restored byte-for-byte.

Intermediate logs are not acceptance evidence: compile errors, invalid initial fixtures, and raw-tree-versus-live-only expectation mismatches are preserved. `native-green.log` covers the first six corrected cases; `allocation-failure2.log` covers the initial fault witness. `final-red.log` uses the final strengthened fixture, not the earlier assertions.

Full YAML SHA256 remains `a3e2c76e91a05c7c1a7ccd6fea56d670939b50411d697645257eade4611d162b`; full workspace CSV remains `5226fd785d56ba528ba5d5a50a08206cf62034e48a9b522cbe254c5c401ea567`; labeled benchmark source remains `bb12331194546dbd91ae1ef5e0b5ba0d3fc141ffd93ba8b71ed956b4b76acbc7`. Generated lockfile churn was restored.

## Remaining blockers

**Public integration is not implemented.** `insert_forward_out_edge` still surfaces bypass `SpillNeedsPromotion`. Conversion currently publishes bucket mode before the separate promoter runs; a later target-allocation failure therefore cannot preserve the original bypass descriptor through naive conversion/promotion/retry. Preparation and publication must be coordinated before routing that signal. The new bucket-level failure witness does not validate the combined transition, arena exhaustion, or first/second canonical-write atomicity.

**Cascade ownership defect reproduced before promotion.** The initial single-orientation fixture seeded 67 rows, enabled bypass, then appended through ordinary cascade-enabled insertion. At the end:

- active initialized prefix: `[256,261)`;
- free span: `[256,267)`;
- reported pinned range: start 272, length 2069;
- next two-slot promotion root allocation: `[256,258)`, overlapping the still-referenced source prefix.

`ownership-diagnostic.log` records free-list state at seed, bypass conversion, append completion and reverse conversion. The free-list conflict appears during appends, not reverse conversion. `cascade-fixture.rs` preserves that fixture. The draft promoter's subsequent source retirement then failed; it is not a successful promotion witness. This is an upstream allocation/relocation ownership violation, not a reason to ignore double-release errors or exclude source spans ad hoc in the promoter.

The actual deferred `insert_forward_out_edge` fixture uses its existing non-cascade write path and does not reproduce this conflict: prefix starts at 256, pinned range starts at 256, and the free list is empty before promotion. Tests now exercise that requested public producer. The cascade counterexample remains open rather than being relabeled as fixed.

No public integration commit, disposable-main native comparison, mixed-hub optimization, full benchmark refresh, PocketIC, or broader atomicity/space approval is claimed. Dirty reference checkouts and historical capacity diagnostics were not modified.
