# Batch value-span replacement and rollback have executable regression guards

**Status: Implemented in the spill worktree; scoped checks and independent re-review complete.**

The [0371 review](adr0097-value-span-retirement-review.md) requested direct batch coverage of the
shared value-span publisher and retirement reservation. Six native tests now exercise the reachable
paths. They also exposed real prefix-write, vertex-accounting and rollback-ownership defects; the
existing batch owner is repaired rather than weakening those assertions. **173 affected tests pass.**
The subsequent commit authorization is limited to this reviewed slice. No push, layout change or
automatic-fold policy is included.

## Reachability and invariant owners

`preflight_run` admits Slab, SlabHoles or Tree. Its overflow exits return `SpillRunDeferred`
unconditionally under ADR0097 §7. `preflight_expanded_run` has no callers; ExpandedSlab originates
there, and RelocatedSlab derives from it. The original request to exercise those two destinations,
and its RelocatedSlab-only publisher counterexample, are therefore inapplicable to current admission.
They are not counted as passing coverage. No fabricated reservation or revival of inactive machinery
is used; deleting that disconnected machinery remains outside this bounded repair.

The [active storage contract](../storage/lara.md#4-free-span-contract-core-lara) names the same owners:

- `reserve_one_orientation_batch_prepared` reserves release capacity before allocator mutation.
  Only New/Reallocated need a record, at most one per run: source retirement on commit **or**
  restoration of a consumed free prefix on rollback. GrowInPlace and tail rewind need none.
- `BatchReservation::commit_with_location_mode` writes Slab values after the resident prefix, not
  at the replacement base. Returned physical locations use that same pending-value offset.
  Its existing pre-write pass accumulates checked allocated bytes and relative edge coverage per
  owner. It resolves the edge base through `labeled_edge_base_from_first_bucket`; the vertex locator
  addresses descriptors, and its degree counts descriptors, not edges. One final owner publication
  preserves actual relocated geometry. Duplicate allocation updates in both inactive specialized
  commit methods are removed before adding this common rule.
- `publish_bucket_inline_property_bytes_span` remains the descriptor-first, source-retirement-second
  boundary; no second publisher is added.
- `rollback_inline_property_bytes_tail` serves both failed reservation and explicit token rollback.
  It restores only consumed free prefixes and rewinds the implicit tail. Registering the rewound
  range as free as well would let a retry allocate the same bytes twice. That registration and the
  corresponding extra `+1` reservation are removed. The tail-reset API comment now states this rule.

No public API, production helper, type, module, flag, dependency, schema or MemoryId is added.
Fresh state is required; previously aliased experimental state is not repaired.

## Named witnesses and observable counterexamples

All tests are in `crates/ic-stable-lara/src/labeled/graph/batch_write.rs`, using existing native graph,
value-payload, row-readback and FailpointMemory helpers. Six fresh graph constructions are added;
zero new MemoryManagers, PocketIC constructors or canister installations. Benchmark execution below
uses the existing canbench fixture, not a new integration-test bootstrap.

| Scenario | Observable contract |
| --- | --- |
| `batch_value_span_relocation_slab_8` / `_33` | Real nonempty, non-tail source; exact Reallocated preflight and Slab token; distinct prefix/pending/peer rows in both orders; returned locations, descriptor count and relative vertex cover; exact source-offset reuse without tail growth; poison leaves live data intact |
| `batch_value_span_relocation_holes_8` / `_33` | Same checks for SlabHoles; token must contain hole index `[1]` and one tail slot; exact replacement rows and logical/physical pending locations |
| `batch_value_span_multiple_runs_share_vertex_counts` | Two distinct labels on one owner accumulate 16 bytes, retain both descriptors and existing cover, and return distinct rows; last-run-wins accounting fails |
| `batch_value_span_reservation_failure_and_rollback` | One fixture with GrowInPlace/Reallocated/New on three owners in separate leaves; actual free-record boundary where one release fits but two require grow; exact backing-grow ResizeFailed and unchanged 16 pre-existing backing prefixes, descriptors, vertices, rows, header and statistics |

The last fixture also proves: a GrowInPlace-only reservation/rollback attempts no record grow;
512 pending rows return exact `SpillRunDeferred` without mutation; explicit rollback returns two
consumed free prefixes and rewinds an appended tail; restored scratch can be poisoned without live
aliasing; forced third-allocation failure returns exact ResizeFailed after earlier reservations and restores
the exact `free_byte_spans()` list; retry destinations are disjoint and commit the exact expected
rows and per-owner byte totals.
Full backing-byte equality is claimed for the two pre-allocation rejection boundaries, not for
internal allocator writes during compensating rollback.

`red-contract.log` records five behavioral failures on the initial candidate, including Slab prefix
corruption and missing vertex accounting. `red-rollback-alias.log` isolates a retry reserving
overlapping destinations `(8200,8216)` and `(8208,8216)`. The final affected run includes all six
witnesses: **173 passed, 0 failed, 451 filtered; runtime 2.07s** (`affected-rereview.log`), up from the 167
selected in 0371.
No old assertion is removed or weakened.

Eight narrowly restored wrong implementations each compile and fail a behavioral assertion:

| Raw log | Regression detected |
| --- | --- |
| `mutant-publisher-runtime.log` | Direct descriptor write bypasses the reachable common publisher: four source-retirement failures |
| `mutant-reserve-new.log` | Omitting New lets reservation succeed instead of the expected backing-grow failure |
| `mutant-reserve-reallocated.log` | Omitting Reallocated does the same; New cannot mask it at the one-versus-two-record boundary |
| `mutant-rollback-double-owner.log` | Registering the rewound tail as free breaks rollback/ownership assertions |
| `mutant-append-prefix.log` | Writing pending Slab values at span base corrupts exact rows at both widths |
| `mutant-owner-accumulation.log` | Replacing per-owner accumulation with per-run projection records 8 instead of 16 bytes |
| `mutant-absolute-cover.log` | Using absolute edge end as relative cover records 261/262 instead of 36 slots |
| `mutant-partial-prefix-new-oracle.log` | Returning the consumed prefix at the wrong offset fails exact free-span equality after partial allocation failure |

All eight exit 101 from runtime assertions; source snapshots and restoration are retained. Preliminary
`initial-witnesses.log` and `mutant-publisher.log` compilation failures are **not** behavioral red
or mutation evidence. The latter was corrected once at its result-type mismatch before the recorded
runtime mutation. Final source matches `affected-rereview.rs` and `rereview/` byte-for-byte.

The first Spec re-review found that partial-failure compensation checked aggregate statistics but
not free-span identity. Its exact counterexample was executable: restoring the consumed 16-byte
prefix at `scratch + 64` instead of `scratch` passes the old oracle
(`mutant-partial-prefix-old-oracle.log`, 1 pass). Two test-only lines now snapshot and compare the
entire free-span list. The same mutation fails that assertion; no production change was needed.
This supplements, rather than replaces, the existing disjoint-retry and data checks.

## Cost and code growth

The existing `bench_l_b_uo_hf_256` measures reserve+commit for 128 unordered replacements after
256 seeded edges and 128 removals, with value width zero. Both runs use the same frozen fixture,
compiler, lock and PocketIC runtime; no benchmark source or persisted result is changed.

| Measurement | Preserved 0371 Wasm | Repaired batch owner | Delta |
| --- | ---: | ---: | ---: |
| Wasm instructions | 113,608 | 115,810 | +2,202 (+1.94%) |
| Heap page increase | 0 | 0 | 0 |
| Stable page increase | 0 | 0 | 0 |

This is net batch-repair cost, including required per-owner accounting/coverage checks and publication,
not an isolated allocator microbenchmark. The increase is accepted for correctness, not presented as
an optimization. Value-bearing batch cost and repeated append/fold/read cost are not measured here.
The 0371 fold tables remain pinned historical measurements, not reruns of this Wasm. The later
free-span-identity assertion changes only `#[cfg(test)]` code; the measured production prefix is
byte-identical, so that test-strengthening does not require another canbench run.

Lizard 1.24.0 (`--csv -i -1`) compares the initial and final `batch_write.rs` snapshots. The five
changed production functions total **1,673→1,684 NLOC, CCN 108→108**. The main commit method grows
540→590 NLOC and 33→34 CCN while allocation duplication is removed from specialized methods
(207→192 and 263→243 NLOC); reservation/rollback also shrink. No signature grows. Six tests and
three focused test helpers add 551 function NLOC / 30 CCN. The longest new fixture is 279 NLOC,
11 CCN: it shares one costly free-record-boundary setup across error, rollback and retry assertions.
The relocation helper takes width and placement, preventing four copies of the same oracle.
No framework or unrelated complexity refactor is introduced.

## Reproduction and receipts

Evidence root (`E`): `$GLEAPH_EVIDENCE/adr0097/batch-value-span-0372`.
Base HEAD: `14430cc622394028503702905db42ad11d6ff029`, branch `spill-swap`.
Plan: `plans/0372-batch-value-span-regressions.md` (ignored). `initial.json`, `protected.json` and
`before/` separate this slice from 269 initially dirty paths, including 265 protected paths.
The environment is **not** a clean checkout. All protected allocator/benchmark dependencies remain
frozen; none is attributed to this change.

Rust/check commands use `python3 E/run.py <unique-name> <cap> <cwd> <command...>`, normal target
locations, <=300s per command and <=590s aggregate. `commands.json` records exact terminal receipts.
Cargo/canbench borrow the same pinned common lock and restore the original bytes after each command.
Pins remain those in 0371 `version-pins.log`: rustc/cargo 1.100.0-nightly, canbench 0.7.0,
PocketIC 13.0.0, Lizard 1.24.0, Apple M2 Pro / Darwin arm64.

| Check | Terminal evidence |
| --- | --- |
| Scoped rustfmt `--check`, edition 2024, `skip_children=true`, both changed Rust files | exit 0; `format-rereview.log` |
| `cargo test --locked --offline -p ic-stable-lara --lib --features canbench -- --nocapture labeled::graph::values::tests labeled::graph::batch_write labeled::graph::compact::tests labeled::graph::insert::tests fold_economics` | exit 0, 173 passed; `affected-rereview.log` |
| Eight focused source mutations | runtime exit 101 each; logs above, exact source snapshots; wrong-offset control passes the old oracle |
| `cargo clippy --locked --offline -p ic-stable-lara --lib --tests --features canbench --message-format=json` | exit 0; 45 emitted diagnostics at 39 distinct existing locations, zero unmatched after mapping changed line numbers against the 41-location baseline; `lint-rereview.json` |
| `canbench bench_l_b_uo_hf_256 --csv --show-canister-output --runtime-path $REPO/crates/ic-stable-lara/.canbench/pocket-ic` | exit 0 twice, one export each; `canbench-base.log`, `canbench-current.log`; fresh evidence directories, no `--persist` |
| Lizard before/current snapshots | exit 0; `lizard-base.log`, `lizard-rereview.log`; original production summary in `lizard-summary.json` |

Preserved baseline Wasm SHA256:
`3b66c0eb683b285315494675d155fb31d687c7d93f5dbc31f30838ba6de58b40`.
Current Wasm SHA256:
`131cc58e665d3c156f7ccdcdf6e035a05ed4b6325a0a62bf5cb29dbdd7bd77f4`.
Baseline/current CSV SHA256:
`948b4f996d749b3212357b7d0bd89a9f1747b73c8046a488c42dbd64e04af7a7` /
`ebcd2287150871e3d16f169ea0c273846677123d523c7951e57d7aa67132a0fc`.

The first review launches exceeded the launcher time cap through nested delegation. Their late
reports are retained, not accepted as completed reviews: Standards reported no source finding but
could not complete receipt inspection; Spec found the free-span-identity P2 above and likewise left
receipt inspection incomplete after a shell-read error. The launcher is corrected to disable
re-delegation and terminate its entire owned process group on timeout, not just the Node parent.
The next flat passes approved the code but encountered macOS `git`/`xcrun` temporary-cache attempts;
a further Spec pass stopped immediately on that refusal. Those are retained as non-acceptance
receipts, not hidden retries. Final file-only reviewers use verified base snapshots, current files,
raw receipts and hashes without the Git launcher or heredocs.

**Final Standards: APPROVE, 0 findings. Final Spec: APPROVE, 0 findings.**
`review-standards-v4.md` and `review-spec-v4.md`, their JSONL transcripts and receipt JSONs record
10 read-only calls each, normal exit 0, and 129.34s / 104.64s. Neither reviewer executed validation,
created files, delegated, or attempted the problematic temporary-cache reads. Both independently
matched all six scope hashes, current/tested source, protected files and raw mutation results. The
partial-prefix P2 is closed by the exact before/after free-list assertion and old/new mutation replay.

Validation-closure receipts are `verify-final.log` / `final.json` and `plan-final.log`: exact six-path scope,
265 protected paths, original lock, empty index and HEAD retained; prior numeric tables, unrelated
storage hunks and document links preserved; final plan structural validation passes. The ignored
plan is the only plan addition. No commit had been made at that checkpoint. The subsequent user
request authorizes a scoped commit, excluding unrelated dirty hunks; no main-worktree edit or push
is authorized.

Strict `-D warnings` remains blocked by GAP-LARA-TEST-CLIPPY; no warning is suppressed. Full library/workspace, PocketIC lifecycle, reopen/upgrade faults and value-bearing
batch benchmarks are not claimed. Only after acceptance may repeated-fold measurements begin;
those measurements will inform, not silently define, an automatic-fold threshold.
